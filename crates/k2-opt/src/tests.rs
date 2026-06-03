//! Per-pass unit tests. Each test lowers a small self-contained k2 program to
//! MIR, runs the optimizer (or a single pass via the public [`optimize`]), and
//! asserts both the expected transform happened (via [`OptStats`] and structural
//! probes) AND that [`MirProgram::verify`] still holds.
//!
//! We drive passes through the public [`optimize`] entry point under a chosen
//! [`OptLevel`], because that is exactly the path the compiler uses; the stats
//! tell us which transforms fired. A handful of tests inspect the resulting MIR
//! structure directly (block counts, presence of a `Call`, check counts).

use k2_mir::{lower_program, BuildMode, MirProgram, Rvalue, Statement};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;

use crate::{optimize, optimize_for_mode, OptLevel};

/// Runs a program on the VM and returns `(stdout, exit_code)`. Used by the
/// regression tests that assert opt == unopt observable behavior.
fn run_stdout(prog: &MirProgram) -> (Vec<u8>, i32) {
    let (_outcome, code, out, _err, _count) = k2_vm::run_metered(prog);
    (out, code)
}

/// Lowers a source string under `mode`, asserting the front-end is clean and the
/// MIR verifies.
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
    let prog = lower_program(&pres.file, &resolved, typed, mode).expect("lowering must succeed");
    assert!(
        prog.verify().is_empty(),
        "input MIR must verify: {:?}",
        prog.verify()
    );
    prog
}

/// Counts the total statements across every function (a size probe).
fn total_stmts(prog: &MirProgram) -> usize {
    prog.funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .map(|b| b.stmts.len())
        .sum()
}

/// Counts direct `Call` rvalues across the program (a devirtualization probe).
fn total_calls(prog: &MirProgram) -> usize {
    prog.funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.stmts.iter())
        .filter(|s| {
            matches!(
                s,
                Statement::Assign {
                    rvalue: Rvalue::Call { .. },
                    ..
                } | Statement::Eval {
                    rvalue: Rvalue::Call { .. },
                    ..
                }
            )
        })
        .count()
}

// =========================================================================
//  Constant folding
// =========================================================================

#[test]
fn const_fold_arithmetic() {
    // `1 + 2 * 3` is comptime-foldable to 7; after optimization the program is
    // verifiable and folding fired.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: u32 = 1;
    const b: u32 = 2;
    const c: u32 = 3;
    const r = a + b * c;
    try out.print("{d}\n", .{r});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseFast);
    let stats = optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(stats.const_folded > 0, "folding must fire: {stats:?}");
}

#[test]
fn const_fold_branch_prunes_dead_arm() {
    // A constant `if (true)` should fold to an unconditional path and the dead
    // arm's block should be collected, reducing the block count.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const flag = true;
    if (flag) {
        try out.print("yes\n", .{});
    } else {
        try out.print("no\n", .{});
    }
}
"#;
    let debug = lower(src, BuildMode::Debug);
    let debug_blocks = debug.block_count();

    let mut prog = lower(src, BuildMode::ReleaseFast);
    let stats = optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        stats.blocks_removed > 0,
        "a dead arm must be removed: {stats:?}"
    );
    assert!(
        prog.block_count() < debug_blocks,
        "optimized block count {} should be < debug {}",
        prog.block_count(),
        debug_blocks
    );
}

#[test]
fn fold_kernel_masks_comptime_result_to_sized_destination() {
    // REGRESSION (v0.9-#3), kernel level: the fold kernel masked a folded
    // Binary/Unary result using only the rvalue's OWN result repr
    // (`int_repr_of(ty)`). When that type is an unsized `comptime_int` (width 0,
    // no masking) and the value is stored into a sized local, the VM masks to the
    // DESTINATION repr via `result_repr`'s comptime->dest fallback; the optimizer
    // did not. We now thread `dst_ty` and mirror `result_repr` exactly. This test
    // drives the kernel directly with a comptime result type and a sized i8
    // destination so it pins the fallback regardless of whether the front-end can
    // ever produce such a shape (which the type checker's range-checks make hard).
    use crate::consts::{fold_binary, fold_unary, int_repr_of};
    use k2_mir::{BinOp, Const, UnOp};
    use k2_types::{IntBits, Type, TypeArena};

    let mut arena = TypeArena::new();
    let comptime = arena.intern(Type::ComptimeInt);
    let i8_ty = arena.intern(Type::Int {
        signed: true,
        bits: IntBits::Fixed(8),
    });
    assert_eq!(
        int_repr_of(&arena, comptime).width,
        0,
        "comptime is unsized"
    );
    assert_eq!(int_repr_of(&arena, i8_ty).width, 8, "i8 is width 8");

    // `100 + 100 = 200`, a comptime-typed Binary, stored into an i8 local. The VM
    // would mask 200 to i8 -> -56. With the fix the folder masks to the i8
    // destination identically; without it the folder kept the unmasked 200.
    let lhs = Const::Int {
        value: 100,
        ty: comptime,
    };
    let rhs = Const::Int {
        value: 100,
        ty: comptime,
    };
    let folded = fold_binary(&arena, BinOp::Add, &lhs, &rhs, comptime, Some(i8_ty))
        .expect("an integer add folds");
    match folded {
        Const::Int { value, .. } => assert_eq!(
            value, -56,
            "a comptime Add into an i8 destination must mask to the i8 repr (200 -> -56)"
        ),
        other => panic!("expected a folded Int, got {other:?}"),
    }

    // `~0 = -1`; into an i8 the bit pattern is the i8 value -1. (`~0` masked to i8
    // is `0xFF` == -1.) A unary BitNot exercises the same fallback.
    let zero = Const::Int {
        value: 0,
        ty: comptime,
    };
    let bitnot =
        fold_unary(&arena, UnOp::BitNot, &zero, comptime, Some(i8_ty)).expect("a bitnot folds");
    match bitnot {
        Const::Int { value, .. } => {
            assert_eq!(value, -1, "~0 masked to i8 is -1, not the unmasked !0")
        }
        other => panic!("expected a folded Int, got {other:?}"),
    }

    // Sanity: with NO sized destination (an `Eval`, dst_ty = None) a comptime
    // result is NOT masked — it stays the full-width value, matching the VM.
    let unmasked = fold_binary(&arena, BinOp::Add, &lhs, &rhs, comptime, None).expect("folds");
    match unmasked {
        Const::Int { value, .. } => assert_eq!(value, 200, "no destination -> no masking"),
        other => panic!("expected a folded Int, got {other:?}"),
    }
}

#[test]
fn const_fold_comptime_into_sized_local_matches_unopt() {
    // REGRESSION (v0.9-#3), end to end: a comptime-typed unary stored into a small
    // sized local must run identically optimized vs unoptimized. `-x` where
    // `x: i8 = -128` is a comptime-typed `neg` flowing into an i8 destination — a
    // case that fires the comptime->sized masking fallback. Both runs are
    // ReleaseFast (no Debug overflow check), isolating constant folding as the only
    // variable; the optimized output must equal the unoptimized output exactly.
    let src = r#"
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    const x: i8 = -128;
    const y: i8 = -x;
    try o.print("{d}\n", .{y});
}
"#;
    let unopt = lower(src, BuildMode::ReleaseFast);
    let (out_unopt, code_unopt) = run_stdout(&unopt);

    let mut opt = lower(src, BuildMode::ReleaseFast);
    let stats = optimize(&mut opt, OptLevel::Fast);
    assert!(opt.verify().is_empty(), "verify after opt");
    assert!(stats.const_folded > 0, "the neg must fold: {stats:?}");
    let (out_opt, code_opt) = run_stdout(&opt);
    assert_eq!(
        out_opt, out_unopt,
        "the optimizer must mask the comptime result to the i8 destination exactly \
         as the VM does (opt {out_opt:?} != unopt {out_unopt:?})"
    );
    assert_eq!(code_opt, code_unopt, "exit codes must match");
}

// =========================================================================
//  Copy / constant propagation + DCE
// =========================================================================

#[test]
fn copy_prop_and_dce_shrink_program() {
    // A chain of copies feeding one use should propagate and the dead copies be
    // collected, shrinking the statement count vs. the unoptimized MIR.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: u32 = 41;
    const b = a;
    const c = b;
    const r = c + 1;
    try out.print("{d}\n", .{r});
}
"#;
    let mut unopt = lower(src, BuildMode::ReleaseFast);
    // Identity baseline: OptLevel::None changes nothing.
    let none_stats = optimize(&mut unopt, OptLevel::None);
    assert_eq!(none_stats, Default::default(), "None is identity");
    let baseline = total_stmts(&unopt);

    let mut prog = lower(src, BuildMode::ReleaseFast);
    let stats = optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        stats.copies_propagated > 0,
        "copies must propagate: {stats:?}"
    );
    assert!(
        stats.stmts_removed > 0,
        "dead stores must be removed: {stats:?}"
    );
    assert!(
        total_stmts(&prog) < baseline,
        "optimized stmt count {} should be < baseline {}",
        total_stmts(&prog),
        baseline
    );
}

// =========================================================================
//  CFG simplification
// =========================================================================

#[test]
fn simplify_cfg_reduces_blocks_on_loops() {
    // An iterative loop has forwarding/empty blocks that simplify should fold.
    let src = r#"
fn sumTo(n: u32) u32 {
    var s: u32 = 0;
    var i: u32 = 0;
    while (i < n) {
        s = s + i;
        i = i + 1;
    }
    return s;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{d}\n", .{sumTo(5)});
}
"#;
    let debug = lower(src, BuildMode::Debug);
    let debug_blocks = debug.block_count();

    let mut prog = lower(src, BuildMode::ReleaseFast);
    optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        prog.block_count() <= debug_blocks,
        "optimized block count {} should be <= debug {}",
        prog.block_count(),
        debug_blocks
    );
}

// =========================================================================
//  Inlining / devirtualization
// =========================================================================

#[test]
fn inlining_devirtualizes_small_forwarder() {
    // A tiny monomorphic helper called once should be inlined away, removing the
    // call (and, after dead-function elimination, the callee may be dropped).
    let src = r#"
fn addOne(x: u32) u32 {
    return x + 1;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: u32 = 10;
    try out.print("{d}\n", .{addOne(a)});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseFast);
    let before_calls = total_calls(&prog);
    let stats = optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        stats.calls_inlined > 0,
        "the helper must be inlined: {stats:?}"
    );
    assert!(
        total_calls(&prog) < before_calls,
        "inlining must remove a call: {} -> {}",
        before_calls,
        total_calls(&prog)
    );
}

#[test]
fn recursive_inlining_terminates() {
    // A recursive `fib` must not be inlined into oblivion; optimization must
    // terminate and verify.
    let src = r#"
fn fib(n: u32) u32 {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{d}\n", .{fib(10)});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseFast);
    optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    // A recursive call must remain somewhere (we did not fully unroll it away).
    assert!(total_calls(&prog) > 0, "the recursive call must survive");
}

#[test]
fn recursion_cycle_optimizes_quickly_and_stays_bounded() {
    // REGRESSION (v0.9-#1): an 8-function mutual-recursion cycle, each function
    // `return next(n-1)+next(n-1)+next(n-1)`, used to blow up inlining: the
    // per-pass recursion accounting reset every outer pass and the size gate
    // sized callees from a stale summary, so a marginally larger SCC pushed
    // compile time toward minutes and produced thousands of MIR blocks. With
    // program-global accounting + a live callee-size gate + a resume-scan, it now
    // optimizes near-instantly to a small, bounded body. We assert the optimized
    // program verifies, stays bounded, and is byte-identical to the unoptimized
    // run across Debug/ReleaseSafe/ReleaseFast.
    let src = r#"
fn f7(n: u32) u32 { if (n == 0) return 8; return f0(n-1)+f0(n-1)+f0(n-1); }
fn f6(n: u32) u32 { if (n == 0) return 7; return f7(n-1)+f7(n-1)+f7(n-1); }
fn f5(n: u32) u32 { if (n == 0) return 6; return f6(n-1)+f6(n-1)+f6(n-1); }
fn f4(n: u32) u32 { if (n == 0) return 5; return f5(n-1)+f5(n-1)+f5(n-1); }
fn f3(n: u32) u32 { if (n == 0) return 4; return f4(n-1)+f4(n-1)+f4(n-1); }
fn f2(n: u32) u32 { if (n == 0) return 3; return f3(n-1)+f3(n-1)+f3(n-1); }
fn f1(n: u32) u32 { if (n == 0) return 2; return f2(n-1)+f2(n-1)+f2(n-1); }
fn f0(n: u32) u32 { if (n == 0) return 1; return f1(n-1)+f1(n-1)+f1(n-1); }
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    try o.print("{d}\n", .{f0(4)});
}
"#;

    // Unoptimized baseline output (the ground truth).
    let unopt = lower(src, BuildMode::ReleaseFast);
    let (out_unopt, code_unopt) = run_stdout(&unopt);
    assert_eq!(out_unopt, b"405\n", "the cycle must compute 405");

    // Optimize under ReleaseFast and assert the body stays bounded. Before the fix
    // this produced thousands of blocks; the program collapses to well under 500.
    let mut fast = lower(src, BuildMode::ReleaseFast);
    let stats = optimize(&mut fast, OptLevel::Fast);
    assert!(
        fast.verify().is_empty(),
        "verify after opt: {:?}",
        fast.verify()
    );
    assert!(
        stats.calls_inlined > 0,
        "inlining must still happen on the cycle: {stats:?}"
    );
    assert!(
        fast.block_count() < 500,
        "the optimized cycle must stay bounded (got {} blocks)",
        fast.block_count()
    );

    // Output is byte-identical to the unoptimized run, across all three modes.
    let (out_fast, code_fast) = run_stdout(&fast);
    assert_eq!(out_fast, out_unopt, "ReleaseFast output must match unopt");
    assert_eq!(code_fast, code_unopt, "ReleaseFast exit must match unopt");

    let mut safe = lower(src, BuildMode::ReleaseSafe);
    optimize(&mut safe, OptLevel::Safe);
    assert!(safe.verify().is_empty(), "verify after safe opt");
    let (out_safe, code_safe) = run_stdout(&safe);
    assert_eq!(out_safe, out_unopt, "ReleaseSafe output must match unopt");
    assert_eq!(code_safe, code_unopt, "ReleaseSafe exit must match unopt");

    let debug = lower(src, BuildMode::Debug);
    let (out_debug, code_debug) = run_stdout(&debug);
    assert_eq!(out_debug, out_unopt, "Debug output must match");
    assert_eq!(code_debug, code_unopt, "Debug exit must match");
}

// =========================================================================
//  Safety-check elimination (ReleaseSafe)
// =========================================================================

#[test]
fn release_safe_eliminates_provable_bound_check() {
    // A constant index into a constant-length array has a provably-true bounds
    // check; ReleaseSafe should remove it. A runtime-length access keeps its
    // check (covered by the differential/bench tests).
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = [_]u8{ 10, 20, 30 };
    const i: usize = 1;
    try out.print("{d}\n", .{a[i]});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseSafe);
    let stats = optimize(&mut prog, OptLevel::Safe);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        stats.checks_removed > 0,
        "a provable bounds check must be removed: {stats:?}"
    );
}

#[test]
fn release_safe_keeps_unprovable_check() {
    // Summing a slice whose elements are runtime values keeps the overflow/bounds
    // checks: they are not provably redundant.
    let src = r#"
fn sumSlice(xs: []const u32) u32 {
    var total: u32 = 0;
    for (xs) |x| {
        total += x;
    }
    return total;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const arr = [_]u32{ 1, 2, 3, 4, 5 };
    try out.print("{d}\n", .{sumSlice(&arr)});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseSafe);
    optimize(&mut prog, OptLevel::Safe);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert!(
        prog.check_count() > 0,
        "ReleaseSafe must keep non-redundant checks; found none"
    );
}

#[test]
fn release_fast_has_no_checks() {
    // ReleaseFast never has checks (stripped at lowering); the optimizer keeps it
    // that way.
    let src = r#"
fn sumSlice(xs: []const u32) u32 {
    var total: u32 = 0;
    for (xs) |x| {
        total += x;
    }
    return total;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const arr = [_]u32{ 1, 2, 3, 4, 5 };
    try out.print("{d}\n", .{sumSlice(&arr)});
}
"#;
    let mut prog = lower(src, BuildMode::ReleaseFast);
    optimize(&mut prog, OptLevel::Fast);
    assert!(prog.verify().is_empty(), "verify after opt");
    assert_eq!(prog.check_count(), 0, "ReleaseFast must have no checks");
}

// =========================================================================
//  Identity / mode mapping
// =========================================================================

#[test]
fn debug_mode_is_identity() {
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: u32 = 1;
    const b: u32 = 2;
    try out.print("{d}\n", .{a + b});
}
"#;
    let mut prog = lower(src, BuildMode::Debug);
    let before = total_stmts(&prog);
    let stats = optimize_for_mode(&mut prog);
    assert_eq!(stats, Default::default(), "Debug must be identity");
    assert_eq!(total_stmts(&prog), before, "Debug MIR must be unchanged");
}
