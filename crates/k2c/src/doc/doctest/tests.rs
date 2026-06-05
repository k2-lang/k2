//! Unit tests for doc-test extraction, classification, and the example-wrapping /
//! import-hoisting machinery.

use super::*;

#[test]
fn classify_modes() {
    assert_eq!(ExMode::classify(""), ExMode::Run);
    assert_eq!(ExMode::classify("k2"), ExMode::Run);
    assert_eq!(ExMode::classify("k2,no_run"), ExMode::NoRun);
    assert_eq!(ExMode::classify("no-run"), ExMode::NoRun);
    assert_eq!(ExMode::classify("k2,compile_fail"), ExMode::CompileFail);
    assert_eq!(ExMode::classify("compile-fail"), ExMode::CompileFail);
    assert_eq!(ExMode::classify("ignore"), ExMode::Ignore);
    assert_eq!(ExMode::classify("text"), ExMode::Ignore);
    // A foreign language tag is rendered but not run.
    assert_eq!(ExMode::classify("rust"), ExMode::Ignore);
    assert_eq!(ExMode::classify("c"), ExMode::Ignore);
}

#[test]
fn extract_counts_and_modes() {
    let md = "intro\n\n```k2\nA\n```\n\nmore\n\n```k2,no_run\nB\n```\n\n```text\nC\n```";
    let examples = extract_examples("f", md);
    assert_eq!(examples.len(), 3);
    assert_eq!(examples[0].mode, ExMode::Run);
    assert_eq!(examples[0].code.trim(), "A");
    assert_eq!(examples[1].mode, ExMode::NoRun);
    assert_eq!(examples[2].mode, ExMode::Ignore);
}

#[test]
fn split_leading_import_parsing() {
    // A whole-line import: name parsed, no trailing code.
    let (name, stmt, tail) = split_leading_import("const std = @import(\"std\");").unwrap();
    assert_eq!(name, "std");
    assert_eq!(stmt, "const std = @import(\"std\");");
    assert_eq!(tail, "");

    let (name, _, tail) = split_leading_import("const foo = @import(\"./bar.k2\");").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(tail, "");

    // An import followed by trailing code: only the import is the statement; the
    // `test` block stays in the tail (so it is scored as the example's own test).
    let (name, stmt, tail) =
        split_leading_import("const std = @import(\"std\"); test \"d\" { _ = 1; }").unwrap();
    assert_eq!(name, "std");
    assert_eq!(stmt, "const std = @import(\"std\");");
    assert_eq!(tail.trim(), "test \"d\" { _ = 1; }");

    // Non-imports.
    assert!(split_leading_import("const x = 1;").is_none());
    assert!(split_leading_import("var y = @import(\"std\");").is_none());
}

#[test]
fn hoist_leading_imports_only() {
    let (imports, rest) =
        hoist_imports("const std = @import(\"std\");\nconst v = add(1,2);\n_ = v;");
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].0, "std");
    assert!(rest.contains("const v = add"));
    assert!(!rest.contains("@import"));
}

#[test]
fn file_top_level_names_from_ast_not_text() {
    // A doc-comment line containing a look-alike `const std = …` must NOT count as
    // a real top-level binding.
    let src = "/// const std = @import(\"std\");\npub fn add(a: i32, b: i32) i32 { return a + b; }";
    let names = file_top_level_names(src);
    assert!(names.contains(&"add".to_string()));
    assert!(
        !names.contains(&"std".to_string()),
        "doc-comment matched: {names:?}"
    );
}

#[test]
fn build_combined_wraps_bare_body() {
    let combined = build_combined_source(
        "pub fn add(a: i32, b: i32) i32 { return a + b; }",
        "const v: i32 = add(1, 2);\n_ = v;",
        "add",
        0,
    );
    assert!(
        combined.source.contains("test \"doc add #0\""),
        "no wrapper: {}",
        combined.source
    );
    assert!(combined.source.contains("pub fn add"));
    assert!(
        matches!(combined.contribution, Contribution::Wrapped(_)),
        "bare body should be wrapped"
    );
}

#[test]
fn build_combined_hoists_std_to_top_level() {
    let combined = build_combined_source(
        "pub fn add(a: i32, b: i32) i32 { return a + b; }",
        "const std = @import(\"std\");\ntry std.testing.expectEqual(@as(i32, 3), add(1, 2));",
        "add",
        0,
    );
    // The `const std` import must appear at file scope, BEFORE the test wrapper.
    let std_pos = combined
        .source
        .find("const std = @import")
        .expect("std import");
    let test_pos = combined.source.find("test \"doc").expect("test wrapper");
    assert!(
        std_pos < test_pos,
        "std not hoisted above the test: {}",
        combined.source
    );
}

#[test]
fn classify_body_is_structural_not_substring() {
    // A `test ` / `fn main` mention inside a STRING or COMMENT must NOT be taken as
    // the example providing its own test/main (the old substring bug). Such a body
    // is `Neither` and gets wrapped, so its statements run inside a test.
    let in_string = "const label = \"run the test suite\";\n_ = label;\nconst v = 1;\n_ = v;";
    assert!(
        matches!(classify_body(in_string), BodyShape::Neither),
        "`test ` in a string literal misclassified as own-test"
    );
    let in_comment = "// call this from fn main in your program\nconst v = 1;\n_ = v;";
    assert!(
        matches!(classify_body(in_comment), BodyShape::Neither),
        "`fn main` in a comment misclassified as own-main"
    );
    // A real `test` block is detected.
    assert!(matches!(
        classify_body("test \"t\" { const v = 1; _ = v; }"),
        BodyShape::Tests(_)
    ));
    // A real `main` (no test) is detected.
    assert!(matches!(
        classify_body("pub fn main(sys: *System) void { _ = sys; }"),
        BodyShape::Main
    ));
}

#[test]
fn example_tests_are_uniquely_renamed() {
    // An example whose own `test` collides BY NAME with a file test must be renamed
    // so the verdict scores the example's test, not the file's.
    let combined = build_combined_source(
        "pub fn f() void {}\ntest \"shared\" { @as(void, undefined); }",
        "test \"shared\" { const v = 1; _ = v; }",
        "f",
        2,
    );
    // The example's test got a unique synthetic name; the file's `"shared"` is
    // untouched in the spliced source.
    let names = match &combined.contribution {
        Contribution::OwnTests(ns) => ns.clone(),
        other => panic!("expected OwnTests, got a different shape: {other:?}"),
    };
    assert_eq!(names.len(), 1);
    assert!(
        names[0].contains("__doctest_") && names[0] != "\"shared\"",
        "example test not uniquely renamed: {names:?}"
    );
    // The renamed name appears in the spliced source; the original `"shared"` from
    // the FILE is still present (only the example's copy was renamed).
    assert!(combined.source.contains(&names[0].replace('"', "")));
}

// =========================================================================
//  Doc-test VERDICT correctness (no false pass / no false fail)
// =========================================================================

/// Builds a single-example [`DocExample`] for the verdict tests below.
fn example(code: &str, mode: ExMode) -> DocExample {
    DocExample {
        item_name: "f".to_string(),
        index: 0,
        code: code.to_string(),
        mode,
        passed: None,
    }
}

#[test]
fn main_example_that_traps_is_failed_not_false_passed() {
    // A `pub fn main` example whose body traps at runtime MUST be reported FAILED:
    // the test harness never runs `main`, so we run it directly. (The old code
    // spliced `main` verbatim, never ran it, and reported a green PASS.)
    let file = "pub fn f() void {}";
    let body = "pub fn main(sys: *System) void { _ = sys; \
                const xs = [_]i32{1,2,3}; const i: usize = 10; _ = xs[i]; }";
    let res = run_one("f", &example(body, ExMode::Run), file, false);
    assert!(!res.passed, "trapping main-example must FAIL: {res:?}");
    assert!(
        res.reason.contains("trap") || res.reason.contains("index"),
        "reason should name the trap: {}",
        res.reason
    );
}

#[test]
fn passing_main_example_passes() {
    let file = "pub fn f() void {}";
    let body = "pub fn main(sys: *System) void { _ = sys; const x: i32 = 1; _ = x; }";
    let res = run_one("f", &example(body, ExMode::Run), file, false);
    assert!(res.passed, "valid main-example must PASS: {res:?}");
}

#[test]
fn unrelated_failing_file_test_does_not_poison_example() {
    // The documented file has its OWN failing `test`; a perfectly valid example
    // (here a `main`-shaped one that contributes no test) must still PASS — the
    // file's failing test must not leak into the example's verdict.
    let file = "const std = @import(\"std\");\n\
                pub fn helper() void {}\n\
                test \"unrelated file test\" { try std.testing.expect(false); }";
    let body = "pub fn main(sys: *System) void { _ = sys; }";
    let res = run_one("helper", &example(body, ExMode::Run), file, false);
    assert!(
        res.passed,
        "an unrelated failing FILE test poisoned the example: {res:?}"
    );
}

#[test]
fn name_collision_example_is_judged_on_its_own_test() {
    // The file has `test "shared"` that FAILS; the example brings its OWN
    // `test "shared"` that PASSES. The example must be judged on its own test
    // (PASS), not the file's same-named failing one.
    let file = "const std = @import(\"std\");\n\
                pub fn f() void {}\n\
                test \"shared\" { try std.testing.expect(false); }";
    let body = "const std = @import(\"std\"); \
                test \"shared\" { try std.testing.expect(true); }";
    let res = run_one("f", &example(body, ExMode::Run), file, false);
    assert!(
        res.passed,
        "name-collision example judged on the file's test, not its own: {res:?}"
    );

    // Symmetric: a file `test "shared"` that PASSES must not mask an example's own
    // same-named test that FAILS.
    let file_ok = "const std = @import(\"std\");\n\
                   pub fn f() void {}\n\
                   test \"shared\" { try std.testing.expect(true); }";
    let body_bad = "const std = @import(\"std\"); \
                    test \"shared\" { try std.testing.expect(false); }";
    let res2 = run_one("f", &example(body_bad, ExMode::Run), file_ok, false);
    assert!(
        !res2.passed,
        "a passing file test masked the example's own failing test: {res2:?}"
    );
}

#[test]
fn compile_fail_marker_is_honored() {
    let file = "pub fn f() void {}";
    // A genuinely non-compiling example tagged compile_fail PASSES.
    let bad = "const x: i32 = \"not an int\"; _ = x;";
    let res = run_one("f", &example(bad, ExMode::CompileFail), file, false);
    assert!(res.passed, "compile_fail on non-compiling body must PASS");
    // A compiling example tagged compile_fail FAILS.
    let good = "const x: i32 = 1; _ = x;";
    let res2 = run_one("f", &example(good, ExMode::CompileFail), file, false);
    assert!(
        !res2.passed,
        "compile_fail on a COMPILING body must FAIL: {res2:?}"
    );
}

#[test]
fn no_run_marker_compiles_but_does_not_execute() {
    let file = "pub fn f() void {}";
    // A `no_run` example whose body would TRAP if run must still PASS (it compiles).
    let body = "pub fn main(sys: *System) void { _ = sys; \
                const xs = [_]i32{1}; const i: usize = 9; _ = xs[i]; }";
    let res = run_one("f", &example(body, ExMode::NoRun), file, false);
    assert!(
        res.passed,
        "no_run example should compile-only and PASS: {res:?}"
    );
    // A no_run example that does NOT compile must FAIL.
    let bad = "const x: i32 = \"nope\"; _ = x;";
    let res2 = run_one("f", &example(bad, ExMode::NoRun), file, false);
    assert!(!res2.passed, "no_run with a compile error must FAIL");
}

#[test]
fn bare_expression_example_runs_inside_a_wrapper() {
    // A bare-body example with a failing assertion must FAIL (it is wrapped and run,
    // not spliced at file scope).
    let file = "const std = @import(\"std\");\npub fn f() void {}";
    let body = "try std.testing.expectEqual(@as(i32, 1), @as(i32, 2));";
    let res = run_one("f", &example(body, ExMode::Run), file, false);
    assert!(
        !res.passed,
        "failing bare-body assertion must FAIL: {res:?}"
    );
    assert!(
        res.reason.contains("assertion") || res.reason.contains("expect"),
        "reason should name the assertion failure: {}",
        res.reason
    );
}
