//! Integration tests for the v0.24 `k2c test` runner machinery at the VM API
//! level: structured per-test outcomes (pass / failed-expect / leak / escaped
//! error), line/function coverage, and the deterministic fuzzer.
//!
//! These drive [`k2_vm::run_tests_opts`] directly over a lowered `MirProgram`, so
//! they assert on the structured [`k2_vm::FailKind`]/[`k2_vm::Coverage`] rather
//! than rendered text — the byte-exact CLI rendering is covered by `k2c`'s
//! `tests/cli.rs`.

use k2_mir::{lower_program, BuildMode};
use k2_parse::{parse, ParseResult};
use k2_resolve::resolve_file;
use k2_syntax::{Expr, Item, SourceFile};
use k2_types::check_file;
use k2_vm::{run_tests_opts, FailKind, RunOpts};

/// Parses `source` with the bundled std prelude injected (user source first so
/// spans stay in user coordinates), re-pointing `@import("std")` at the std root.
fn parse_with_std(source: &str) -> ParseResult {
    let mut combined = String::with_capacity(source.len() + k2_std::STD_BODY.len() + 64);
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());
    let mut result = parse(&combined);
    rewrite_std_imports(&mut result.file);
    result
}

/// Re-points every `const X = @import("std")` to the synthetic std root.
fn rewrite_std_imports(file: &mut SourceFile) {
    for item in &mut file.items {
        if let Item::Const { value, .. } = item {
            if import_target(value).as_deref() == Some("std") {
                let span = value.span();
                *value = Expr::Ident {
                    name: k2_std::STD_ROOT_NAME.to_string(),
                    span,
                };
            }
        }
    }
}

/// If `e` is exactly `@import("name")`, returns the imported name.
fn import_target(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                return Some(text.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Lowers a source string (with std injected) to a verified `MirProgram`.
fn lower(src: &str) -> k2_mir::MirProgram {
    let pres = parse_with_std(src);
    assert!(pres.is_ok(), "parse errors: {:?}", pres.diagnostics);
    let resolved = resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve errors: {:?}",
        resolved.diagnostics
    );
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors: {:?}", typed.diagnostics);
    let prog =
        lower_program(&pres.file, &resolved, typed, BuildMode::Debug).expect("lowering succeeds");
    let problems = prog.verify();
    assert!(problems.is_empty(), "malformed MIR: {problems:?}");
    prog
}

/// The user/std boundary for coverage (the std prelude is appended, so the user
/// source length is the cut).
fn boundary(src: &str) -> u32 {
    src.chars().count() as u32
}

#[test]
fn passing_suite_all_pass() {
    let src = r#"
        const std = @import("std");
        test "a" { try std.testing.expectEqual(@as(i32, 4), 2 + 2); }
        test "b" { try std.testing.expect(true); }
        test "strings" { try std.testing.expectEqualStrings("ok", "ok"); }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(report.passed(), 3);
    assert_eq!(report.failed(), 0);
    assert!(report.results.iter().all(|r| r.passed));
}

#[test]
fn failed_expect_has_failkind_and_span() {
    let src = r#"
        const std = @import("std");
        test "bad" { try std.testing.expectEqual(@as(i32, 5), 2 + 2); }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(report.failed(), 1);
    let f = report.results[0].failure.as_ref().expect("a failure");
    assert_eq!(f.kind, FailKind::FailedExpect);
    assert!(
        f.message.contains("expected 5, found 4"),
        "message: {}",
        f.message
    );
    // The span points at the assertion line (line 3 of the source, 1-based).
    let span = f.span.expect("a failure span");
    assert_eq!(span.line, 3, "caret should land on the assertion line");
}

#[test]
fn leak_has_leak_failkind() {
    // The allocation escapes into a returned struct, so static analysis cannot
    // flag it — the runtime testing-allocator leak check must.
    let src = r#"
        const std = @import("std");
        const Holder = struct { buf: []u8 };
        fn makeHolder(alloc: std.mem.Allocator) !Holder {
            const a = try alloc.alloc(u8, 8);
            return Holder{ .buf = a };
        }
        test "leak" {
            const h = try makeHolder(std.testing.allocator);
            _ = h;
        }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(report.failed(), 1);
    let f = report.results[0].failure.as_ref().expect("a failure");
    assert_eq!(f.kind, FailKind::Leak, "expected a leak failure");
}

#[test]
fn defer_free_passes_no_false_leak() {
    let src = r#"
        const std = @import("std");
        test "clean" {
            var alloc = std.testing.allocator;
            const a = try alloc.alloc(u8, 8);
            defer alloc.free(a);
            try std.testing.expectEqual(@as(usize, 8), a.len);
        }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(
        report.passed(),
        1,
        "a freed alloc must not be flagged a leak"
    );
    assert_eq!(report.failed(), 0);
}

#[test]
fn coverage_hit_subset_of_total_and_excludes_std() {
    let src = r#"
        const std = @import("std");
        fn dbl(x: i32) i32 { return x * 2; }
        fn unused(x: i32) i32 { return x + 1; }
        test "t" { try std.testing.expectEqual(@as(i32, 8), dbl(4)); }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        coverage: true,
        user_boundary: boundary(src),
        ..RunOpts::default()
    };
    let report = run_tests_opts(&prog, &opts);
    let cov = report.coverage.expect("coverage collected");
    // Hit lines are a subset of the total executable user lines.
    assert!(
        cov.lines_hit.keys().all(|l| cov.lines_total.contains(l)),
        "every hit line must be in the denominator"
    );
    // `dbl` is covered; `unused` is not — so coverage is partial.
    assert!(cov.lines_hit.len() < cov.lines_total.len());
    assert!(cov.fns_hit.len() < cov.fns_total);
    // The std prelude (after the boundary) is excluded: the user fn count is small
    // (dbl + unused; test fns are synthetic and excluded).
    assert!(
        cov.fns_total <= 2,
        "std fns must be excluded; got {} user fns",
        cov.fns_total
    );
}

#[test]
fn coverage_is_deterministic() {
    let src = r#"
        const std = @import("std");
        fn dbl(x: i32) i32 { return x * 2; }
        test "t" { try std.testing.expectEqual(@as(i32, 8), dbl(4)); }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        coverage: true,
        user_boundary: boundary(src),
        ..RunOpts::default()
    };
    let a = run_tests_opts(&prog, &opts).coverage.unwrap();
    let b = run_tests_opts(&prog, &opts).coverage.unwrap();
    assert_eq!(a.lines_hit, b.lines_hit);
    assert_eq!(a.lines_total, b.lines_total);
    assert_eq!(a.fns_hit, b.fns_hit);
}

#[test]
fn fuzz_finds_planted_bug_at_a_stable_iteration() {
    let src = r#"
        const std = @import("std");
        fn trapsAt(x: u64) !void {
            if ((x & 0xFF) == 0x42) { unreachable; }
        }
        test "fuzz: trapsAt" {
            const x = std.testing.fuzzInput();
            try trapsAt(x);
        }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        fuzz: true,
        fuzz_runs: 256,
        ..RunOpts::default()
    };
    let r1 = run_tests_opts(&prog, &opts);
    assert_eq!(r1.failed(), 1, "the planted bug must be found");
    let f1 = r1.results[0].failure.as_ref().unwrap();
    assert_eq!(f1.kind, FailKind::Trap);
    assert!(f1.message.contains("fuzz iteration"));
    // Reproducible: the same seed finds it at the same iteration.
    let r2 = run_tests_opts(&prog, &opts);
    let f2 = r2.results[0].failure.as_ref().unwrap();
    assert_eq!(
        f1.message, f2.message,
        "fuzz failure must reproduce exactly"
    );
}

#[test]
fn fuzz_clean_target_passes() {
    let src = r#"
        const std = @import("std");
        fn neverTraps(x: u64) !void { _ = x; }
        test "fuzz: neverTraps" {
            const x = std.testing.fuzzInput();
            try neverTraps(x);
        }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        fuzz: true,
        fuzz_runs: 256,
        ..RunOpts::default()
    };
    let report = run_tests_opts(&prog, &opts);
    assert_eq!(report.passed(), 1);
    assert_eq!(report.results[0].fuzz_runs, Some(256));
}

#[test]
fn filter_selects_a_subset() {
    let src = r#"
        const std = @import("std");
        test "alpha" { try std.testing.expect(true); }
        test "beta"  { try std.testing.expect(true); }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        filters: vec!["beta".to_string()],
        ..RunOpts::default()
    };
    let report = run_tests_opts(&prog, &opts);
    assert_eq!(report.results.len(), 1);
    assert!(report.results[0].name.contains("beta"));
}

// ===========================================================================
//  v0.24 milestone regression tests (the verified review findings).
// ===========================================================================

/// [BLOCKER] The per-VM instruction budget must be RESET per test, so a suite of
/// many work-heavy tests all pass — a later test can never spuriously FAIL with
/// "instruction budget exhausted" merely because earlier tests consumed the shared
/// budget. (Before the fix this reported `2 passed, 3 failed`.)
#[test]
fn budget_is_reset_per_test_so_a_heavy_suite_all_passes() {
    // Each test runs a real loop that burns a meaningful number of instructions;
    // summed across all five they would exceed the budget if it were not reset.
    // The running value is masked to a small range so the additions never overflow.
    let src = r#"
        const std = @import("std");
        fn busy() u64 {
            var acc: u64 = 0;
            var i: u64 = 0;
            while (i < 200000) : (i += 1) { acc = (acc + (i & 0xFF)) & 0xFFFF; }
            return acc;
        }
        test "h1" { _ = busy(); try std.testing.expect(true); }
        test "h2" { _ = busy(); try std.testing.expect(true); }
        test "h3" { _ = busy(); try std.testing.expect(true); }
        test "h4" { _ = busy(); try std.testing.expect(true); }
        test "h5" { _ = busy(); try std.testing.expect(true); }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(
        report.passed(),
        5,
        "every correct test must pass; a later test must not inherit an exhausted budget"
    );
    assert_eq!(report.failed(), 0);
}

/// [BLOCKER] A genuinely non-terminating test still trips the (now per-test) budget
/// and fails ALONE — its neighbors, run on a fresh budget, pass. This proves the
/// reset did not weaken the termination guarantee.
#[test]
fn infinite_test_fails_alone_neighbors_pass() {
    // `spin` loops forever; the budget (reset per test) trips it. The surrounding
    // tests get their own full budget and pass.
    let src = r#"
        const std = @import("std");
        fn spin() u64 {
            var i: u64 = 0;
            while (true) : (i = (i + 1) & 0xFFFF) {}
            return i;
        }
        test "ok before" { try std.testing.expect(true); }
        test "infinite" { _ = spin(); try std.testing.expect(true); }
        test "ok after" { try std.testing.expect(true); }
    "#;
    let prog = lower(src);
    let report = run_tests_opts(&prog, &RunOpts::default());
    assert_eq!(report.passed(), 2, "the two finite tests must pass");
    assert_eq!(report.failed(), 1, "only the infinite test fails");
    let infinite = report
        .results
        .iter()
        .find(|r| r.name.contains("infinite"))
        .expect("the infinite test");
    let f = infinite.failure.as_ref().expect("a failure");
    assert_eq!(f.kind, FailKind::Trap);
    assert!(
        f.message.contains("budget"),
        "the infinite test must trip the budget; got: {}",
        f.message
    );
}

/// [MAJOR] Line coverage must be attributed per EXECUTING function: a never-called
/// user function's line must NOT be credited just because a test body (excluded
/// from the denominator) executed on the same physical line number. (cov9: the
/// single line holds `never_run` + a test body; `never_run` is never called.)
#[test]
fn coverage_line_not_credited_by_excluded_test_body_on_same_line() {
    let src =
        "const std = @import(\"std\"); fn never_run() i32 { return 999; } test \"t\" { try std.testing.expect(true); }";
    let prog = lower(src);
    let opts = RunOpts {
        coverage: true,
        user_boundary: boundary(src),
        ..RunOpts::default()
    };
    let cov = run_tests_opts(&prog, &opts).coverage.expect("coverage");
    // `never_run` is the only user function and it never executes.
    assert_eq!(cov.fns_total, 1);
    assert!(cov.fns_hit.is_empty(), "never_run is never called");
    // Its single line must therefore be UNCOVERED — the line report must agree with
    // the function report (0/1), not over-report 1/1 because the test body aliased it.
    let (hit, total) = cov.line_counts();
    assert_eq!(total, 1, "exactly one user code point");
    assert_eq!(
        hit, 0,
        "the never-run line must not be credited by the test body"
    );
}

/// [MAJOR] Two user functions on ONE physical line, only one called: per-(function,
/// line) attribution reports 1/2, matching the function report — not an over-counted
/// 1/1 by bare line number.
#[test]
fn coverage_two_fns_one_line_reports_half() {
    let src = "const std = @import(\"std\");\nfn f() i32 { return 10; } fn g() i32 { return 20; }\ntest \"t\" { try std.testing.expectEqual(@as(i32, 10), f()); }";
    let prog = lower(src);
    let opts = RunOpts {
        coverage: true,
        user_boundary: boundary(src),
        ..RunOpts::default()
    };
    let cov = run_tests_opts(&prog, &opts).coverage.expect("coverage");
    assert_eq!(cov.fns_total, 2);
    assert_eq!(cov.fns_hit.len(), 1, "only f is called");
    let (hit, total) = cov.line_counts();
    assert_eq!(
        (hit, total),
        (1, 2),
        "g's shared line is uncovered: 1/2, not 1/1"
    );
}

/// [MAJOR] `expectEqualSlices` on EQUAL-length but DIFFERENT-content slices must
/// name the first differing index + elements (not the useless "expected N, found N"
/// of the two lengths), and a length mismatch must name both lengths.
#[test]
fn expect_equal_slices_reports_index_and_length_mismatches() {
    // Content mismatch at index 1.
    let content = r#"
        const std = @import("std");
        test "content" {
            try std.testing.expectEqualSlices(u32, &[_]u32{ 1, 2, 3 }, &[_]u32{ 1, 9, 3 });
        }
    "#;
    let prog = lower(content);
    let report = run_tests_opts(&prog, &RunOpts::default());
    let f = report.results[0].failure.as_ref().expect("a failure");
    assert_eq!(f.kind, FailKind::FailedExpect);
    assert!(
        f.message.contains("index 1")
            && f.message.contains("expected 2")
            && f.message.contains("found 9"),
        "content mismatch must name index 1 (2 vs 9); got: {}",
        f.message
    );
    assert!(
        !f.message.contains("expected 3, found 3"),
        "must not render the old length-based 'expected 3, found 3'; got: {}",
        f.message
    );

    // Length mismatch names both lengths.
    let length = r#"
        const std = @import("std");
        test "length" {
            try std.testing.expectEqualSlices(u32, &[_]u32{ 1, 2, 3 }, &[_]u32{ 1, 2 });
        }
    "#;
    let prog = lower(length);
    let report = run_tests_opts(&prog, &RunOpts::default());
    let f = report.results[0].failure.as_ref().expect("a failure");
    assert!(
        f.message.contains("lengths differ") && f.message.contains("3") && f.message.contains("2"),
        "length mismatch must name both lengths; got: {}",
        f.message
    );
}

/// [NIT] `--fuzz-runs=0` must never silently PASS an unexercised fuzz target: the VM
/// driver reports it as a failure (the CLI rejects it earlier with a friendlier
/// message; this is the API-level defense).
#[test]
fn fuzz_zero_runs_is_not_a_silent_pass() {
    let src = r#"
        const std = @import("std");
        fn neverTraps(x: u64) !void { _ = x; }
        test "fuzz: neverTraps" {
            const x = std.testing.fuzzInput();
            try neverTraps(x);
        }
    "#;
    let prog = lower(src);
    let opts = RunOpts {
        fuzz: true,
        fuzz_runs: 0,
        ..RunOpts::default()
    };
    let report = run_tests_opts(&prog, &opts);
    assert_eq!(report.passed(), 0, "0 iterations must not read as a pass");
    assert_eq!(report.failed(), 1);
    let f = report.results[0].failure.as_ref().expect("a failure");
    assert!(
        f.message.contains("0 iterations") || f.message.contains(">= 1"),
        "must explain the 0-run misconfiguration; got: {}",
        f.message
    );
}

/// [MINOR] Fuzz determinism: a target that traps on EVERY input is caught at the
/// FIRST iteration regardless of seed — a deterministic regression target (not the
/// seed-fragile 1/256 corpus bug). Verified across several seeds.
#[test]
fn fuzz_guaranteed_trigger_is_seed_independent() {
    let src = r#"
        const std = @import("std");
        fn alwaysTraps(x: u64) !void {
            _ = x;
            unreachable;
        }
        test "fuzz: alwaysTraps" {
            const x = std.testing.fuzzInput();
            try alwaysTraps(x);
        }
    "#;
    let prog = lower(src);
    for seed in [0u64, 1, 0xDEAD_BEEF, 0x2545_F491_4F6C_DD1D] {
        let opts = RunOpts {
            fuzz: true,
            fuzz_runs: 256,
            seed,
            ..RunOpts::default()
        };
        let report = run_tests_opts(&prog, &opts);
        assert_eq!(
            report.failed(),
            1,
            "a guaranteed-trigger bug must be found for seed {seed:#x}"
        );
        // Found at the very first iteration (every input trips it): the annotated
        // failure message names iteration 0 regardless of seed.
        let f = report.results[0].failure.as_ref().expect("a failure");
        assert!(
            f.message.contains("fuzz iteration 0"),
            "the always-trap bug must be caught at iteration 0 for seed {seed:#x}; got: {}",
            f.message
        );
    }
}
