//! DOC-TESTS: extract fenced code blocks from doc comments, compile + run each as
//! a real `test` block via the existing VM harness, and report pass/fail.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! A doc example is compiled IN THE CONTEXT of the documented file (the file's
//! pub items are spliced in before the example) so an example can reference them
//! by name — exactly how a reader runs it. Each example is compiled in its own
//! combined source so a compile error is attributed precisely and never
//! cross-contaminates a sibling example.
//!
//! Reuse map (no edits to the compile pipeline): [`crate::parse_program`] →
//! [`resolve_file`] → [`check_file`] → [`lower_program`] → [`crate::run_optimizer`]
//! → [`run_tests_opts`]. The run is under [`BuildMode::Debug`] so the safety checks
//! and the leak-checking allocator stay live: an example that traps, mis-asserts,
//! leaks, or lets an error escape is a FAIL. A `compile_fail` example PASSES iff it
//! does NOT compile, which is what turns a "does-not-compile" example into a
//! *result* rather than a crash.

use std::io::{self, Write};
use std::process::ExitCode;

use k2_mir::{lower_program, BuildMode};
use k2_resolve::resolve_file;
use k2_types::check_file;
use k2_vm::{run_captured, run_tests_opts, FailKind, RunArgs, RunOpts, RunOutcome};

use super::render::scan_blocks;
use super::{DocExample, DocModel};

/// How an example's info string classifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExMode {
    /// A runnable k2 example (compiled AND executed).
    Run,
    /// Compiled but not executed (`no_run` / `no-run`).
    NoRun,
    /// Expected to FAIL compilation (`compile_fail`); passes iff it does not compile.
    CompileFail,
    /// Skipped entirely (`ignore`, or a non-k2 language tag).
    Ignore,
}

impl ExMode {
    /// Classifies a fenced block's info string. Empty / `k2` → [`ExMode::Run`]; a
    /// recognized directive token selects the matching mode; any other language tag
    /// (`rust`, `c`, `text`, …) → [`ExMode::Ignore`].
    pub fn classify(info: &str) -> ExMode {
        let lower = info.to_ascii_lowercase();
        let tokens: Vec<&str> = lower
            .split([',', ' ', '\t'])
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.iter().any(|t| *t == "ignore" || *t == "text") {
            return ExMode::Ignore;
        }
        if tokens
            .iter()
            .any(|t| *t == "compile_fail" || *t == "compile-fail")
        {
            return ExMode::CompileFail;
        }
        if tokens.iter().any(|t| *t == "no_run" || *t == "no-run") {
            return ExMode::NoRun;
        }
        // The first token, if present, is the language: k2 (or empty) is runnable;
        // anything else is a foreign code block we only render, never run.
        match tokens.first() {
            None => ExMode::Run,
            Some(&"k2") => ExMode::Run,
            // A bare directive with no language (e.g. ```no_run handled above) —
            // here we already filtered directives, so an unknown first token is a
            // foreign language tag.
            Some(_) => ExMode::Ignore,
        }
    }
}

/// Extracts the fenced code blocks of a doc string as [`DocExample`]s owned by
/// `item_name`. Reuses the shared block scanner so fences are parsed identically
/// to the renderer.
pub fn extract_examples(item_name: &str, doc_md: &str) -> Vec<DocExample> {
    let mut examples = Vec::new();
    let mut index = 0;
    for block in scan_blocks(doc_md) {
        if let super::render::Block::Code(info, body) = block {
            let mode = ExMode::classify(&info);
            examples.push(DocExample {
                item_name: item_name.to_string(),
                index,
                code: body,
                mode,
                passed: None,
            });
            index += 1;
        }
    }
    examples
}

/// The outcome of one doc-test.
#[derive(Clone, Debug)]
pub struct DocTestResult {
    /// The display name, e.g. `doc add #0`.
    pub name: String,
    /// `true` iff the example passed (per its mode).
    pub passed: bool,
    /// A human-readable reason on failure (empty on pass / skip).
    pub reason: String,
    /// `true` if the example was skipped (counted toward neither pass nor fail in
    /// the strict sense, but reported as `ignored`).
    pub skipped: bool,
}

/// A flat (item-name, example) reference produced while collecting every example
/// in the model.
struct ExampleRef<'a> {
    /// The owning item / module label.
    name: String,
    /// The example itself.
    example: &'a DocExample,
}

/// Collects every example across the model (file doc, modules, items). The order
/// is deterministic (model order + example index).
fn collect_examples(model: &DocModel) -> Vec<ExampleRef<'_>> {
    let mut refs = Vec::new();
    for ex in &model.file_examples {
        refs.push(ExampleRef {
            name: "(file)".to_string(),
            example: ex,
        });
    }
    for module in &model.modules {
        for ex in &module.examples {
            let label = if module.path.is_empty() {
                "(module)".to_string()
            } else {
                module.path.join(".")
            };
            refs.push(ExampleRef {
                name: label,
                example: ex,
            });
        }
        for item in &module.items {
            for ex in &item.examples {
                refs.push(ExampleRef {
                    name: item.qualified_name(),
                    example: ex,
                });
            }
        }
    }
    refs
}

/// Runs every doc-test in `model` against `file_source` (the original file text,
/// whose pub items the examples reference). Prints per-example status to stderr and
/// a summary to stdout. Returns `SUCCESS` iff no runnable/compile-fail example
/// failed. `force_no_run` (the `--no-run` flag) downgrades every `Run` example to
/// compile-only.
///
/// Also mutates the model: each `DocExample.passed` is set so the HTML emitter can
/// show a pass/fail badge.
pub fn run_doctests(model: &mut DocModel, file_source: &str, force_no_run: bool) -> ExitCode {
    // Compute results against an immutable snapshot of the examples, then write the
    // pass flags back by (name, index) so the borrow checker is happy. Each
    // example's verdict is computed over its OWN contributed code only (its
    // uniquely-renamed test(s), or a directly-run `main`), so the documented file's
    // pre-existing `test` blocks never poison an example's pass/fail.
    let results = {
        let refs = collect_examples(model);
        let mut out = Vec::new();
        for r in &refs {
            out.push((
                r.name.clone(),
                r.example.index,
                run_one(&r.name, r.example, file_source, force_no_run),
            ));
        }
        out
    };

    // Report and tally.
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for (_name, _idx, res) in &results {
        if res.skipped {
            skipped += 1;
            let _ = writeln!(err, "doc-test {} ... ignored", res.name);
            continue;
        }
        if res.passed {
            passed += 1;
            let _ = writeln!(err, "doc-test {} ... ok", res.name);
        } else {
            failed += 1;
            let _ = writeln!(err, "doc-test {} ... FAILED", res.name);
            if !res.reason.is_empty() {
                let _ = writeln!(err, "  {}", res.reason);
            }
        }
    }
    drop(err);

    // Write pass flags back into the model for the HTML badges.
    for module in &mut model.modules {
        let mpath = if module.path.is_empty() {
            "(module)".to_string()
        } else {
            module.path.join(".")
        };
        apply_pass_flags(&mut module.examples, &mpath, &results);
        for item in &mut module.items {
            // The owner key MUST match the one `collect_examples`/`run_one` used:
            // the `.`-joined qualified name (root items use the bare name).
            let qn = if module.path.is_empty() {
                item.name.clone()
            } else {
                format!("{}.{}", module.path.join("."), item.name)
            };
            apply_pass_flags(&mut item.examples, &qn, &results);
        }
    }
    apply_pass_flags(&mut model.file_examples, "(file)", &results);

    let total = passed + failed;
    let _ = writeln!(
        io::stdout(),
        "{passed} passed, {failed} failed, {total} total ({skipped} ignored) (doc-tests)"
    );
    let _ = io::stdout().flush();
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Writes each result's pass flag back onto the matching example by (name, index).
fn apply_pass_flags(
    examples: &mut [DocExample],
    name: &str,
    results: &[(String, usize, DocTestResult)],
) {
    for ex in examples.iter_mut() {
        if ex.mode == ExMode::Ignore {
            continue;
        }
        if let Some((_, _, res)) = results.iter().find(|(n, i, _)| n == name && *i == ex.index) {
            if !res.skipped {
                ex.passed = Some(res.passed);
            }
        }
    }
}

/// Compiles + (maybe) runs one example, producing its [`DocTestResult`]. Never
/// panics: a parse/resolve/type/lower error becomes a `Fail` (or a `Pass` for a
/// `compile_fail`), and the VM run is itself `catch_unwind`-wrapped upstream.
fn run_one(owner: &str, ex: &DocExample, file_source: &str, force_no_run: bool) -> DocTestResult {
    let name = format!("doc {owner} #{}", ex.index);
    let effective = if force_no_run && ex.mode == ExMode::Run {
        ExMode::NoRun
    } else {
        ex.mode
    };

    if effective == ExMode::Ignore {
        return DocTestResult {
            name,
            passed: true,
            reason: String::new(),
            skipped: true,
        };
    }

    let combined = build_combined_source(file_source, &ex.code, &ex.item_name, ex.index);

    // Front-end: parse → resolve → check → lower. Any error here is a compile
    // failure. For `compile_fail`, a compile failure is the PASS condition.
    let compiled = compile_combined(&combined.source);

    match effective {
        ExMode::CompileFail => match compiled {
            Ok(_) => DocTestResult {
                name,
                passed: false,
                reason: "expected the example to fail compilation, but it compiled".to_string(),
                skipped: false,
            },
            Err(_) => DocTestResult {
                name,
                passed: true,
                reason: String::new(),
                skipped: false,
            },
        },
        ExMode::NoRun => match compiled {
            Ok(_) => DocTestResult {
                name,
                passed: true,
                reason: String::new(),
                skipped: false,
            },
            Err(reason) => DocTestResult {
                name,
                passed: false,
                reason: format!("compile-only example failed to compile: {reason}"),
                skipped: false,
            },
        },
        ExMode::Run => {
            let mut prog = match compiled {
                Ok(p) => p,
                Err(reason) => {
                    return DocTestResult {
                        name,
                        passed: false,
                        reason: format!("example failed to compile: {reason}"),
                        skipped: false,
                    }
                }
            };
            // Debug mode keeps checks + the leak tracker; the optimizer stays OFF.
            if let Err(e) = crate::run_optimizer(&mut prog, BuildMode::Debug, false) {
                return DocTestResult {
                    name,
                    passed: false,
                    reason: format!("internal: {e}"),
                    skipped: false,
                };
            }
            run_contribution(name, &prog, &combined.contribution)
        }
        ExMode::Ignore => unreachable!("handled above"),
    }
}

/// Scores a compiled example over its OWN contributed code ONLY — never the
/// documented file's pre-existing tests. The three [`Contribution`] shapes are each
/// judged on exactly what the example brought:
///
/// * `OwnTests` / `Wrapped`: run the test harness, then look at ONLY the example's
///   uniquely-named test(s); the file's own tests (pass or fail) are irrelevant.
/// * `Main`: the harness never runs `main`, so run `main` directly and read its
///   outcome (a trap / escaped error / panic is a FAIL).
fn run_contribution(
    name: String,
    prog: &k2_mir::MirProgram,
    contribution: &Contribution,
) -> DocTestResult {
    // The set of the example's OWN unique test display names (empty for `Main`,
    // which is scored separately below).
    let own: Vec<&str> = match contribution {
        Contribution::OwnTests(ns) => ns.iter().map(String::as_str).collect(),
        Contribution::Wrapped(n) => vec![n.as_str()],
        Contribution::Main => Vec::new(),
    };

    match contribution {
        Contribution::OwnTests(_) | Contribution::Wrapped(_) => {
            let report = run_tests_opts(prog, &RunOpts::default());
            // Score ONLY the example's own test(s), matched by their unique display
            // names. A file test (even a failing one) is never consulted.
            let own_results: Vec<&k2_vm::TestResult> = report
                .results
                .iter()
                .filter(|r| {
                    let display = r.name.strip_prefix("test ").unwrap_or(&r.name);
                    own.contains(&display)
                })
                .collect();

            match own_results.iter().find(|r| !r.passed) {
                Some(r) => {
                    let reason = match &r.failure {
                        Some(f) => format!("{}: {}", fail_label(f.kind), f.message),
                        None => "the example test failed".to_string(),
                    };
                    DocTestResult {
                        name,
                        passed: false,
                        reason,
                        skipped: false,
                    }
                }
                None => DocTestResult {
                    name,
                    passed: true,
                    reason: String::new(),
                    skipped: false,
                },
            }
        }
        Contribution::Main => {
            // The VM test harness never invokes `main`, so run it directly. The leak
            // tracker / safety checks stay live (the program was lowered in Debug),
            // so a trap, an escaped error, or a panic is a FAIL — no false PASS.
            let (outcome, _code, _out, _err) = run_captured(prog, RunArgs::new(BuildMode::Debug));
            match outcome {
                RunOutcome::Ok => DocTestResult {
                    name,
                    passed: true,
                    reason: String::new(),
                    skipped: false,
                },
                RunOutcome::Errored(e) => DocTestResult {
                    name,
                    passed: false,
                    reason: format!("escaped error: {e}"),
                    skipped: false,
                },
                RunOutcome::Panicked(m) => DocTestResult {
                    name,
                    passed: false,
                    reason: format!("trap: {m}"),
                    skipped: false,
                },
            }
        }
    }
}

/// A short label for a failure kind, for the doc-test reason line.
fn fail_label(kind: FailKind) -> &'static str {
    match kind {
        FailKind::FailedExpect => "assertion failed",
        FailKind::Trap => "trap",
        FailKind::EscapedError => "escaped error",
        FailKind::Leak => "leak",
    }
}

/// How an example's own body contributes runnable code to the combined source —
/// decided STRUCTURALLY by parsing the example body (never a substring scan, which
/// would match inside a comment, a string literal, or a longer identifier such as
/// `latest`/`mainline`). The verdict is computed strictly over what the example
/// itself contributes, identified here.
#[derive(Debug)]
enum Contribution {
    /// The example brought its own `test` block(s); the `Vec` holds the UNIQUE
    /// (renamed) display names the harness will report, so they can never collide
    /// with the documented file's own pre-existing tests. The verdict is the AND of
    /// exactly these tests' results.
    OwnTests(Vec<String>),
    /// The example defines a top-level `main` (and no `test`). Since the VM test
    /// harness never invokes `main`, we run `main` directly (via [`run_captured`])
    /// and score it from the run outcome — so a trapping/erroring/panicking
    /// main-example is a FAIL, not a silent false-PASS.
    Main,
    /// The example defines neither a `test` nor a `main`: its body was wrapped in a
    /// synthetic, uniquely-named `test` so the harness discovers and runs it. The
    /// `String` is that unique display name.
    Wrapped(String),
}

/// The combined source for one example plus a description of how the example
/// contributes runnable code (used to score the verdict over the example's OWN
/// code only).
struct Combined {
    /// The full combined program text.
    source: String,
    /// What the example contributes (own tests / a main / a synthetic wrapper).
    contribution: Contribution,
}

/// Builds the combined source for one example: the original file, then the example
/// spliced in.
///
/// The example body is PARSED to decide its shape (mirroring `file_test_names` /
/// `file_top_level_names`, which already use the AST rather than text):
///
/// * If it defines `test` block(s), each is renamed with a unique
///   `__doctest_<item>_<index>_<n>__` prefix (a span-precise rewrite) and spliced
///   verbatim, so the example's tests can never collide with the file's own tests
///   and the verdict is computed over exactly those renamed tests.
/// * If it defines a top-level `main` (and no test), the body is spliced verbatim;
///   the caller runs `main` directly so its RUNTIME behaviour is exercised.
/// * Otherwise the body is wrapped in a uniquely-named synthetic `test` block.
///
/// A `const NAME = @import("…");` is prepended to the example only when the file
/// lacks one and the example references `std`.
fn build_combined_source(file_source: &str, body: &str, item: &str, index: usize) -> Combined {
    let mut out = String::with_capacity(file_source.len() + body.len() + 128);
    out.push_str(file_source);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');

    // A `const NAME = @import("…");` must live at FILE scope: `std.x.y` member
    // access resolves to the intrinsic floor only for a top-level `std` binding,
    // not one declared inside a test body. So hoist any leading import-consts out
    // of the example body to the top level (rustdoc-style: the author may write
    // `const std = @import("std");` and we place it correctly). A name already
    // bound at file scope is dropped to avoid a redeclaration.
    let file_bindings = file_top_level_names(file_source);
    let (imports, rest) = hoist_imports(body);
    let mut hoisted_std = false;
    for (name, line) in &imports {
        // Drop a hoisted import whose name already binds a file-scope item (to
        // avoid a redeclaration). Match on the PARSED item names, never a substring
        // of the source (a doc-comment line could contain `const std = …`).
        if !file_bindings.iter().any(|n| n == name) {
            out.push_str(line);
            out.push('\n');
            if name == "std" {
                hoisted_std = true;
            }
        } else if name == "std" {
            hoisted_std = true;
        }
    }
    // If the body uses `std.` but never imported it (and the file did not), add the
    // canonical top-level binding so the common example compiles. We must NOT add it
    // when the (un-hoisted) body already binds `std` at its own top level — e.g. a
    // one-liner `const std = @import("std"); test "…" { … }` that hoisting
    // deliberately left intact — or the splice would redeclare `std`.
    let file_has_std = file_bindings.iter().any(|n| n == "std");
    let body_binds_std = file_top_level_names(&rest).iter().any(|n| n == "std");
    if rest.contains("std.") && !hoisted_std && !file_has_std && !body_binds_std {
        out.push_str("const std = @import(\"std\");\n");
    }

    // Classify the example STRUCTURALLY by parsing its (import-hoisted) body.
    let shape = classify_body(&rest);
    let contribution = match shape {
        BodyShape::Tests(test_names) => {
            // Rename each example test with a unique prefix so it cannot collide
            // with a file test of the same display name; the rewrite is span-precise
            // (not a substring replace). Splice the renamed body verbatim.
            let (renamed_body, renamed_names) =
                rename_example_tests(&rest, &test_names, item, index);
            out.push_str(&renamed_body);
            if !renamed_body.ends_with('\n') {
                out.push('\n');
            }
            Contribution::OwnTests(renamed_names)
        }
        BodyShape::Main => {
            // A `main`-shaped example: splice verbatim; the caller runs `main`.
            out.push_str(&rest);
            if !rest.ends_with('\n') {
                out.push('\n');
            }
            Contribution::Main
        }
        BodyShape::Neither => {
            // Wrap the body in a uniquely-named synthetic test so a name collision
            // with a file test is impossible and the verdict is unambiguous. The
            // runner's DISPLAY name is the raw token text after `test ` — i.e. the
            // QUOTED string literal — so we store that quoted form for scoring.
            let label = format!("doc {item} #{index}");
            out.push_str(&format!("test \"{label}\" {{\n"));
            out.push_str(&rest);
            if !rest.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("}\n");
            Contribution::Wrapped(format!("\"{label}\""))
        }
    };

    Combined {
        source: out,
        contribution,
    }
}

/// The structural shape of an example body, decided by parsing (NOT substring).
enum BodyShape {
    /// The body defines `test` block(s); the `Vec` holds each test's raw display
    /// name (the text after `test `, e.g. `"my test"` or a bare identifier), in
    /// source order.
    Tests(Vec<String>),
    /// The body defines a top-level `main` function (and no `test`).
    Main,
    /// The body defines neither a `test` nor a `main` (bare items/expressions).
    Neither,
}

/// Parses `body` and classifies it: a `test`-bearing body reports its test names; a
/// `main`-bearing body (with no test) is [`BodyShape::Main`]; anything else is
/// [`BodyShape::Neither`]. Total: a body that does not parse as items is treated as
/// `Neither` (it will be wrapped and the wrapped test will surface the real
/// compile error), never a substring match.
fn classify_body(body: &str) -> BodyShape {
    use k2_syntax::Item;
    let parsed = k2_parse::parse(body);
    let mut test_names = Vec::new();
    let mut has_main = false;
    for it in &parsed.file.items {
        match it {
            Item::Test { name, .. } => test_names.push(name.clone().unwrap_or_default()),
            Item::Fn { name, .. } if name == "main" => has_main = true,
            _ => {}
        }
    }
    if !test_names.is_empty() {
        BodyShape::Tests(test_names)
    } else if has_main {
        BodyShape::Main
    } else {
        BodyShape::Neither
    }
}

/// Renames every `test` block in `body` with a unique synthetic prefix so an
/// example test can never collide (by display name) with the documented file's own
/// tests. Returns `(rewritten_body, new_display_names)`.
///
/// The rewrite is SPAN-PRECISE: each `Item::Test`'s span starts at its `test`
/// keyword, so the name token is the first `"…"`/identifier run after `test` +
/// whitespace. We rewrite from the LAST test to the FIRST so earlier char offsets
/// stay valid. A bare (unnamed) `test { … }` becomes `test "<prefix>" { … }`.
fn rename_example_tests(
    body: &str,
    raw_names: &[String],
    item: &str,
    index: usize,
) -> (String, Vec<String>) {
    use k2_syntax::Item;
    let parsed = k2_parse::parse(body);
    // The test items in source order, paired with the per-test ordinal `n`.
    let tests: Vec<(usize, &Item)> = parsed
        .file
        .items
        .iter()
        .filter(|it| matches!(it, Item::Test { .. }))
        .enumerate()
        .collect();

    let item_slug = super::render::slug(item);
    let chars: Vec<char> = body.chars().collect();
    let mut out_chars = chars.clone();
    let mut new_names = vec![String::new(); raw_names.len()];

    // Rewrite from the last test backwards so earlier offsets are not disturbed.
    for (n, it) in tests.iter().rev() {
        let Item::Test { name, span, .. } = it else {
            continue;
        };
        // The unique display name. We make the QUOTED form (`"…"`) the new name so
        // the runner's display string is exactly `"<unique>"` regardless of whether
        // the original was a string literal, a bare identifier, or absent.
        let unique = format!("__doctest_{item_slug}_{index}_{n}__");
        let quoted = format!("\"{unique}\"");
        new_names[*n] = quoted.clone();

        // Locate the name token (or the insertion point for a bare `test`) within
        // the test item's span. `span.start` is the `test` keyword.
        let start = span.start as usize;
        // Skip `test` (4 scalars) then any whitespace.
        let mut p = start + 4;
        while p < chars.len() && chars[p].is_whitespace() {
            p += 1;
        }
        match name {
            Some(raw) => {
                // The name token occupies `raw.chars().count()` scalars at `p`.
                let len = raw.chars().count();
                let q: Vec<char> = quoted.chars().collect();
                out_chars.splice(p..p + len, q);
            }
            None => {
                // Bare `test { … }`: insert ` "<unique>"` after `test`. We splice at
                // `start + 4` (right after the keyword) so spacing stays clean.
                let ins: Vec<char> = format!(" {quoted}").chars().collect();
                out_chars.splice(start + 4..start + 4, ins);
            }
        }
    }

    (out_chars.into_iter().collect(), new_names)
}

/// Splits an example body into `(import_consts, rest)`. A leading
/// `const NAME = @import("…");` (possibly indented, and possibly followed by more
/// code ON THE SAME LINE) is an import binding that must live at file scope; we
/// collect each as `(name, verbatim_import_stmt)` and return the remaining body.
/// Only leading imports are hoisted — once a non-import construct is seen, the
/// rest of the body is left intact (so an `@import` used mid-body is not disturbed).
///
/// Crucially, when an import is followed by trailing code on the same line (e.g.
/// `const std = @import("std"); test "d" { … }`), ONLY the import statement is
/// hoisted; the trailing `test "d" { … }` stays in `rest` so it is classified and
/// scored as the example's own contribution rather than silently swallowed.
fn hoist_imports(body: &str) -> (Vec<(String, String)>, String) {
    let mut imports = Vec::new();
    let mut rest_lines: Vec<String> = Vec::new();
    let mut still_leading = true;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if still_leading {
            if trimmed.is_empty() {
                continue; // skip blank leading lines
            }
            if let Some((name, stmt, tail)) = split_leading_import(trimmed) {
                imports.push((name, stmt));
                let tail = tail.trim();
                if tail.is_empty() {
                    continue;
                }
                // Trailing code after the import: keep it (and stop leading-import
                // scanning, since a later import would be mid-body).
                still_leading = false;
                rest_lines.push(tail.to_string());
                continue;
            }
            still_leading = false;
        }
        rest_lines.push(line.to_string());
    }
    (imports, rest_lines.join("\n"))
}

/// The names of the file's top-level `const`/`var`/`fn` bindings, from the PARSED
/// AST (never a substring of the raw text — a doc-comment line may contain a
/// look-alike `const std = …`). Used to decide whether a hoisted import would
/// redeclare a file-scope binding.
fn file_top_level_names(file_source: &str) -> Vec<String> {
    use k2_syntax::Item;
    let parsed = k2_parse::parse(file_source);
    parsed
        .file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Const { name, .. } | Item::Var { name, .. } | Item::Fn { name, .. } => {
                Some(name.clone())
            }
            _ => None,
        })
        .collect()
}

/// If `line` STARTS with a `const NAME = @import(...);` binding, returns
/// `(NAME, import_stmt, tail)` where `import_stmt` is the verbatim
/// `const … ;` and `tail` is whatever follows it on the line (often empty). Else
/// `None`. This is what lets a one-liner `const std = @import("std"); test "…" {…}`
/// hoist just the import and keep the trailing `test` in the example body.
fn split_leading_import(line: &str) -> Option<(String, String, &str)> {
    const KW: &str = "const ";
    let rest = line.strip_prefix(KW)?;
    let eq = rest.find('=')?; // byte index of `=` within `rest`
    let name = rest[..eq].trim();
    // The value text after `=` and the `@import(...)` requirement.
    let value_full = &rest[eq + 1..];
    let value = value_full.trim_start();
    if !value.starts_with("@import(") {
        return None;
    }
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    // The import statement ends at the first `;` (byte index within `value`).
    let semi = value.find(';')?;
    // Map back to a byte index within `line`: KW + (eq + 1) + leading-ws-of-value +
    // semi + 1 (past the `;`).
    let ws = value_full.len() - value.len();
    let stmt_end = KW.len() + eq + 1 + ws + semi + 1;
    let stmt = line[..stmt_end].to_string();
    let tail = &line[stmt_end..];
    Some((name.to_string(), stmt, tail))
}

/// Compiles a combined source through the front-end + lowering, returning the MIR
/// program or a one-line error reason. The std prelude is injected by
/// [`crate::parse_program`] (no span shift), so the example sees `@import("std")`.
fn compile_combined(combined: &str) -> Result<k2_mir::MirProgram, String> {
    let pres = crate::parse_program(combined);
    if !pres.is_ok() {
        return Err(first_error(&pres.diagnostics, "parse error"));
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        return Err(first_error(&resolved.diagnostics, "name-resolution error"));
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        return Err(first_error(&typed.diagnostics, "type error"));
    }
    match lower_program(&pres.file, &resolved, typed, BuildMode::Debug) {
        Ok(prog) => {
            // A lowering pass can still report blocking diagnostics.
            if prog.diagnostics.iter().any(|d| d.is_error()) {
                return Err(first_error(&prog.diagnostics, "lowering error"));
            }
            Ok(prog)
        }
        Err(diags) => Err(first_error(&diags, "lowering failed")),
    }
}

/// Extracts the first error-severity diagnostic's message, or a fallback.
fn first_error<D: HasErr>(diags: &[D], fallback: &str) -> String {
    diags
        .iter()
        .find(|d| d.is_error())
        .map(|d| d.message_text())
        .unwrap_or_else(|| fallback.to_string())
}

/// A tiny trait abstracting the two diagnostic types we read here (parse/resolve/
/// types share `is_error` + a message field but are distinct structs).
trait HasErr {
    /// `true` for an error-severity diagnostic.
    fn is_error(&self) -> bool;
    /// The human message text.
    fn message_text(&self) -> String;
}

impl HasErr for k2_parse::Diagnostic {
    fn is_error(&self) -> bool {
        self.severity == k2_parse::Severity::Error
    }
    fn message_text(&self) -> String {
        self.message.clone()
    }
}

impl HasErr for k2_resolve::Diagnostic {
    fn is_error(&self) -> bool {
        self.is_error()
    }
    fn message_text(&self) -> String {
        self.message.clone()
    }
}

impl HasErr for k2_types::Diagnostic {
    fn is_error(&self) -> bool {
        self.is_error()
    }
    fn message_text(&self) -> String {
        self.message.clone()
    }
}

impl HasErr for k2_mir::Diagnostic {
    fn is_error(&self) -> bool {
        self.is_error()
    }
    fn message_text(&self) -> String {
        self.message.clone()
    }
}

#[cfg(test)]
mod tests;
