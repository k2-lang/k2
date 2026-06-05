//! The first-class `k2c test` runner (v0.24): discover, compile, and run every
//! `test "name" { ... }` block of a file (and its imports), reporting per-test
//! ok/FAIL with a v0.20 caret diagnostic, a `N passed, M failed, K total`
//! summary, an exit code, plus optional line/function COVERAGE and a deterministic
//! FUZZ mode.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! ## How it works
//!
//! Discovery is STRUCTURAL, not textual: a `test` block in any reachable module
//! lowers to a MIR function named `"test <name>"`, so once the program is compiled
//! the test set falls out of the VM's `run_tests_opts`. Two compile paths feed it:
//!
//! * **Single file / stdin** (no `@import("./x.k2")` path imports) — the std
//!   prelude is injected by the lowerer (NOT prepended as text), so every span
//!   stays in *user* coordinates: a FAIL caret lands on the exact assertion line in
//!   the user's own file. This is the acceptance path.
//! * **Path imports** — the module graph is merged into one source via
//!   [`crate::multi::merge`]; spans are merged-text coordinates, rendered against
//!   the merged source.
//!
//! Each test runs in isolation on a fresh fiber with the leak-checking testing
//! allocator; a failed `expect*`, a trap, a leak, or an escaped error is a FAIL.
//! The runner is DETERMINISTIC: test order is the MIR lowering order, coverage uses
//! ordered maps + integer-only percentages, and fuzzing uses a fixed-seed PRNG.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use k2_mir::{lower_program, BuildMode};
use k2_resolve::resolve_file;
use k2_syntax::{Label, RichDiagnostic, RichSeverity, Span};
use k2_types::check_file;
use k2_vm::{
    run_program_coverage, run_tests_opts, Coverage, FailKind, RunArgs, RunOpts, RunOutcome,
    TestFailure,
};

use crate::multi;
use crate::render::{self, RenderOpts};

/// Which coverage sections to report.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CoverageSel {
    /// No coverage.
    Off,
    /// Both line and function coverage.
    Both,
    /// Line coverage only (`--coverage=line`).
    Line,
    /// Function coverage only (`--coverage=func`).
    Func,
}

/// The parsed `k2c test` options.
struct TestArgs {
    /// The file / directory / `-` (stdin) to test.
    path: String,
    /// The build mode (default Debug, checks on — required for leak detection).
    mode: BuildMode,
    /// Name-filter substrings (OR); empty runs all.
    filters: Vec<String>,
    /// Summary-line-only output.
    quiet: bool,
    /// Print every `name ... ok` line, not just FAILs.
    verbose: bool,
    /// The coverage selection.
    coverage: CoverageSel,
    /// Drive fuzz targets.
    fuzz: bool,
    /// The fuzz PRNG seed.
    seed: u64,
    /// Fuzz iterations per target.
    fuzz_runs: usize,
    /// Run only the fenced doc examples (the `--doc` flag), via the doc-test core.
    doc: bool,
}

/// The `test` subcommand entry point.
pub fn cmd_test(args: &[String]) -> Result<ExitCode, String> {
    let parsed = parse_args(args)?;
    if parsed.doc {
        return run_doc_tests(&parsed);
    }
    let p = Path::new(&parsed.path);
    if parsed.path != "-" && p.is_dir() {
        return run_directory(&parsed);
    }
    run_one_source(&parsed)
}

/// `k2c test --doc <file>`: compile + run ONLY the fenced doc examples of the
/// file's doc comments, reusing the doc-test core (no HTML output). The exit code
/// is nonzero iff any runnable/compile-fail example failed.
fn run_doc_tests(parsed: &TestArgs) -> Result<ExitCode, String> {
    let (source, label) = crate::read_source(&parsed.path)?;
    let mut model = crate::doc::build_doc_model(&source, &label)
        .map_err(|reason| format!("cannot document {label}: {reason}"))?;
    Ok(crate::doc::doctest::run_doctests(
        &mut model, &source, false,
    ))
}

/// Parses the `test` flags into a [`TestArgs`].
fn parse_args(args: &[String]) -> Result<TestArgs, String> {
    let mut path: Option<String> = None;
    let mut mode = BuildMode::Debug;
    let mut filters = Vec::new();
    let mut quiet = false;
    let mut verbose = false;
    let mut coverage = CoverageSel::Off;
    let mut fuzz = false;
    let mut seed: u64 = RunOpts::default().seed;
    let mut fuzz_runs: usize = RunOpts::default().fuzz_runs;
    let mut doc = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let a = arg.as_str();
        if let Some(f) = a.strip_prefix("--filter=") {
            filters.push(f.to_string());
            continue;
        }
        if let Some(c) = a.strip_prefix("--coverage=") {
            coverage = match c {
                "line" => CoverageSel::Line,
                "func" | "function" => CoverageSel::Func,
                "both" | "all" => CoverageSel::Both,
                other => return Err(format!("unknown `--coverage` mode `{other}`")),
            };
            continue;
        }
        if let Some(s) = a.strip_prefix("--seed=") {
            seed = parse_u64(s)?;
            continue;
        }
        if let Some(n) = a.strip_prefix("--fuzz-runs=") {
            fuzz_runs = n
                .parse()
                .map_err(|_| format!("`--fuzz-runs` expects a number; got `{n}`"))?;
            continue;
        }
        match a {
            "--filter" => {
                let v = it
                    .next()
                    .ok_or_else(|| "`--filter` needs a value".to_string())?;
                filters.push(v.clone());
            }
            "--seed" => {
                let v = it
                    .next()
                    .ok_or_else(|| "`--seed` needs a value".to_string())?;
                seed = parse_u64(v)?;
            }
            "--fuzz-runs" => {
                let v = it
                    .next()
                    .ok_or_else(|| "`--fuzz-runs` needs a value".to_string())?;
                fuzz_runs = v
                    .parse()
                    .map_err(|_| format!("`--fuzz-runs` expects a number; got `{v}`"))?;
            }
            "--quiet" => quiet = true,
            "--verbose" => verbose = true,
            "--coverage" => coverage = CoverageSel::Both,
            "--fuzz" => fuzz = true,
            "--doc" => doc = true,
            "--release-fast" => mode = BuildMode::ReleaseFast,
            "--release-safe" => mode = BuildMode::ReleaseSafe,
            "--debug" => mode = BuildMode::Debug,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `test` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("`test` takes one path; got extra `{other}`"));
                }
                path = Some(other.to_string());
            }
        }
    }
    let path =
        path.ok_or_else(|| "`test` needs a <file.k2|dir> argument (or `-` for stdin)".to_string())?;
    // A zero fuzz budget would drive a fuzz target ZERO times and then report it as
    // a silent PASS (the `for i in 0..0` loop never runs), masking an untested
    // target. Reject it as a misconfiguration rather than let "0 iterations" read as
    // a green result. (Only meaningful with `--fuzz`, but rejecting it always keeps
    // the contract simple and the error obvious.)
    if fuzz_runs == 0 {
        return Err("`--fuzz-runs` must be >= 1".to_string());
    }
    Ok(TestArgs {
        path,
        mode,
        filters,
        quiet,
        verbose,
        coverage,
        fuzz,
        seed,
        fuzz_runs,
        doc,
    })
}

/// Parses a `u64` seed, accepting `0x…` hex or plain decimal.
fn parse_u64(s: &str) -> Result<u64, String> {
    let r = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse()
    };
    r.map_err(|_| format!("expected a u64 seed; got `{s}`"))
}

/// Runs the test blocks of one source (a file or stdin). For a file with no path
/// imports the std prelude is injected by the lowerer so spans stay in USER
/// coordinates; a path-importing file routes through the merge.
fn run_one_source(parsed: &TestArgs) -> Result<ExitCode, String> {
    let (source, label) = crate::read_source(&parsed.path)?;

    // Path imports require the merge (and a real build root); reject from stdin.
    if multi::has_path_imports(&source) {
        if parsed.path == "-" {
            return Err(
                "cannot resolve a relative `@import(\"./...\")` from stdin; pass a file path"
                    .to_string(),
            );
        }
        return run_merged(parsed, &label);
    }

    // The single-file pipeline: user source first (offsets preserved), std
    // appended. Spans in the user code keep their original line/col.
    let pres = crate::parse_program(&source);
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        return gate(&label, "parse errors");
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        render::emit_errors(&label, &source, &resolved.diagnostics);
        return gate(&label, "resolution errors");
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        render::emit_errors(&label, &source, &typed.diagnostics);
        return gate(&label, "type errors");
    }
    let mut prog = match lower_program(&pres.file, &resolved, typed, parsed.mode) {
        Ok(p) => p,
        Err(diags) => {
            render::emit_errors(&label, &source, &diags);
            return gate(&label, "lowering failed");
        }
    };
    let errors = render::emit_diags(&label, &source, &prog.diagnostics);
    if errors > 0 {
        return gate(&label, "lowering had errors");
    }
    // The optimizer is left OFF in Debug so the safety checks (overflow/bounds)
    // and the leak tracker stay intact — leak detection depends on them.
    crate::run_optimizer(&mut prog, parsed.mode, false)?;
    let problems = prog.verify();
    if !problems.is_empty() {
        for p in &problems {
            let _ = writeln!(io::stderr(), "error: malformed MIR: {}", p.message);
        }
        return Ok(ExitCode::FAILURE);
    }

    // The user/std boundary: a span at/after this offset is std prelude, excluded
    // from the coverage denominator so the percentage reflects user code.
    let boundary = source.chars().count() as u32;
    let opts = run_opts(parsed, boundary);
    let report = run_tests_opts(&prog, &opts);
    // The single-file path keeps user coordinates (std is injected by the lowerer,
    // not merged), so no source map is needed.
    render_report(&label, &source, &report, parsed, None)
}

/// Runs the test blocks of a path-importing program via the module merge. Spans
/// are merged-text coordinates, rendered against the merged source.
fn run_merged(parsed: &TestArgs, label: &str) -> Result<ExitCode, String> {
    let root = Path::new(&parsed.path);
    let build_root = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let merged = multi::merge(&multi::CompileInputs {
        root_source: root.to_path_buf(),
        build_root,
        named_modules: Vec::new(),
        build_options: Vec::new(),
        inject_build: false,
    })
    .map_err(|d| format!("{}: {}", d.label, d.message))?;

    let mut prog = match multi::compile_merged(&merged.source, label, parsed.mode) {
        Ok(p) => p,
        Err(diags) => {
            for d in &diags {
                let _ = writeln!(io::stderr(), "{}: {}", d.label, d.message);
            }
            return gate(label, "front-end errors");
        }
    };
    crate::run_optimizer(&mut prog, parsed.mode, false)?;
    // The merged text interleaves user modules, the std/build roots, and the root
    // file, so a single user/std boundary offset cannot separate them. Instead,
    // exclude prelude code from the coverage denominator by PROVENANCE: pass the
    // std/build char-offset ranges the merge recorded so a function/line defined in
    // them is dropped (no boundary cut). This stops std `expectEqual`/`mem.eql` (and
    // their lines, which were mislabeled with the user filename) from inflating the
    // user coverage report.
    let mut opts = run_opts(parsed, 0);
    opts.prelude_ranges = merged.prelude_ranges.clone();
    let report = run_tests_opts(&prog, &opts);
    // Pass the source map so a FAIL caret / uncovered line is relabeled with the
    // true (file, line) instead of the misleading `<root>:<merged-line>`.
    render_report(
        label,
        &merged.source,
        &report,
        parsed,
        Some(&merged.source_map),
    )
}

/// Runs every `*.k2` under a directory (sorted, deterministic), aggregating the
/// per-file reports into one summary + exit code. A `build.k2`-driven project is
/// out of scope here; use `k2c build test` for the named-module wiring.
fn run_directory(parsed: &TestArgs) -> Result<ExitCode, String> {
    let dir = Path::new(&parsed.path);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading `{}`: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "k2").unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no `.k2` files under `{}`", dir.display()));
    }
    let mut any_failed = false;
    for f in files {
        let sub = TestArgs {
            path: f.to_string_lossy().into_owned(),
            mode: parsed.mode,
            filters: parsed.filters.clone(),
            quiet: parsed.quiet,
            verbose: parsed.verbose,
            coverage: parsed.coverage,
            fuzz: parsed.fuzz,
            seed: parsed.seed,
            fuzz_runs: parsed.fuzz_runs,
            doc: parsed.doc,
        };
        match run_one_source(&sub)? {
            ExitCode::SUCCESS => {}
            _ => any_failed = true,
        }
    }
    if any_failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Builds the [`RunOpts`] from the parsed flags + the user/std boundary. The merged
/// path overrides `prelude_ranges` after building (it excludes prelude by
/// provenance, not by a single boundary).
fn run_opts(parsed: &TestArgs, boundary: u32) -> RunOpts {
    RunOpts {
        filters: parsed.filters.clone(),
        coverage: parsed.coverage != CoverageSel::Off,
        fuzz: parsed.fuzz,
        seed: parsed.seed,
        fuzz_runs: parsed.fuzz_runs,
        user_boundary: boundary,
        prelude_ranges: Vec::new(),
    }
}

/// A small `error: cannot test … : <reason>` gate that prints the reason and
/// returns the failure exit code (no tests run).
fn gate(label: &str, reason: &str) -> Result<ExitCode, String> {
    let _ = writeln!(io::stderr(), "error: cannot test {label}: {reason}");
    Ok(ExitCode::FAILURE)
}

/// Renders the per-test results (FAIL carets to stderr, `ok` lines to stderr in
/// verbose mode), the coverage section (to stdout), and the summary (to stdout).
/// Returns SUCCESS iff every test passed. `smap` (the merged path's source map)
/// recovers the true `(file, line)` of a merged offset so a FAIL caret / uncovered
/// line is labeled with the real source location, not a merged coordinate; `None`
/// for the single-file path, whose spans are already user coordinates.
fn render_report(
    label: &str,
    source: &str,
    report: &k2_vm::RichTestReport,
    parsed: &TestArgs,
    smap: Option<&multi::SourceMap>,
) -> Result<ExitCode, String> {
    let opts = RenderOpts::detect();
    // Stream the tests' own captured output first.
    let _ = io::stdout().write_all(&report.stdout);
    let _ = io::stderr().write_all(&report.stderr);

    let stderr = io::stderr();
    let mut err = stderr.lock();
    for r in &report.results {
        // The raw MIR name is `test "name"`; strip the `test ` prefix to get the
        // display form (which already carries its own quotes).
        let display = r.name.strip_prefix("test ").unwrap_or(&r.name);
        match &r.failure {
            None => {
                if parsed.verbose && !parsed.quiet {
                    match r.fuzz_runs {
                        Some(n) => {
                            let _ = writeln!(err, "test {display} ... ok ({n} fuzz runs)");
                        }
                        None => {
                            let _ = writeln!(err, "test {display} ... ok");
                        }
                    }
                }
            }
            Some(failure) => {
                let _ = writeln!(err, "FAIL: test {display}");
                if !parsed.quiet {
                    let diag = build_fail_diagnostic(display, failure, source, smap);
                    let rendered = render::render(label, source, &diag, &opts);
                    let _ = err.write_all(rendered.as_bytes());
                }
            }
        }
    }
    drop(err);

    // Coverage (to stdout, deterministic).
    if let Some(cov) = &report.coverage {
        print_coverage(label, cov, parsed.coverage, smap);
    }

    let passed = report.passed();
    let failed = report.failed();
    let total = passed + failed;
    let _ = writeln!(
        io::stdout(),
        "{passed} passed, {failed} failed, {total} total"
    );
    let _ = io::stdout().flush();
    if failed == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// Builds the v0.20 rich diagnostic for one failing test: an error header with the
/// failure message, a primary caret over the assertion/trap location, and a note
/// naming the test (and, for a leak, how to fix it). On the merged path `smap`
/// recovers the true `(file, line)` of the merged failure line, added as a note so
/// the header's `<root>:<merged-line>` is not the only (and misleading) location.
fn build_fail_diagnostic(
    display: &str,
    failure: &TestFailure,
    source: &str,
    smap: Option<&multi::SourceMap>,
) -> RichDiagnostic {
    // Resolve the failure span to a real char-offset span against `source` so the
    // caret lands on the right line. `TestFailure.span` carries `(line, col)` with
    // a placeholder offset of 0, so we recompute the offset here.
    let span = failure
        .span
        .map(|s| line_anchored_span(source, s))
        .unwrap_or_else(|| Span::point(0, 1, 1));

    let caret_msg = match failure.kind {
        FailKind::FailedExpect => "assertion failed here",
        FailKind::Trap => "trap fired here",
        FailKind::EscapedError => "error propagated from here",
        FailKind::Leak => "leak detected in this test",
    };
    let mut diag = RichDiagnostic::new(RichSeverity::Error, span, failure.message.clone());
    diag.primary = Label::primary(span, caret_msg);
    diag = diag.note(format!("in test {display}"));
    // Relabel the merged coordinate with the true source location: the rendered code
    // line is correct (it is the merged source line), but the header reads
    // `<root>:<merged-line>`, which can name the wrong file/line for imported code.
    if let Some(sm) = smap {
        if let Some((file, line)) = sm.resolve(span.line) {
            diag = diag.note(format!(
                "at {file}:{line} (merged source line {})",
                span.line
            ));
        }
    }
    if failure.kind == FailKind::Leak {
        diag = diag.note(
            "the testing allocator detected a leak; add a matching free/deinit \
             (e.g. `defer x.deinit()`)",
        );
    }
    diag
}

/// Computes a real, line-anchored span from a `(line, col)` carried in `sp`:
/// finds the char offset of the start of `sp.line`, advances to `sp.col`, and
/// widens the end to end-of-line so the caret underlines the whole statement.
/// Falls back to a point span if the line is out of range.
fn line_anchored_span(source: &str, sp: Span) -> Span {
    if sp.line == 0 {
        return Span::point(0, sp.line.max(1), sp.col.max(1));
    }
    // Char offset of the first char of line `sp.line` (1-based).
    let mut off = 0usize;
    let mut line = 1u32;
    let chars: Vec<char> = source.chars().collect();
    while line < sp.line && off < chars.len() {
        if chars[off] == '\n' {
            line += 1;
        }
        off += 1;
    }
    let line_start = off;
    let start = line_start + (sp.col.saturating_sub(1) as usize);
    // End of line: scan to the next newline (or EOF).
    let mut end = start.min(chars.len());
    while end < chars.len() && chars[end] != '\n' {
        end += 1;
    }
    let start = start.min(chars.len());
    Span::new(start as u32, end.max(start) as u32, sp.line, sp.col.max(1))
}

/// Prints the coverage section (to stdout) with integer-only percentages so the
/// output is bit-identical across runs/machines. Honours the section selection.
/// `smap` (the merged path's source map) relabels each uncovered line with its true
/// `(file, line)`; `None` for the single-file path, whose lines are user coordinates.
fn print_coverage(label: &str, cov: &Coverage, sel: CoverageSel, smap: Option<&multi::SourceMap>) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "coverage:");

    if sel != CoverageSel::Func {
        // Counts come from the per-(function, line) code points so a line shared by
        // two functions counts honestly (covered only when each is executed), never
        // collapsing to one over-credited line. The uncovered LIST below is by line
        // number, which is what a human wants to see.
        let (hit, total) = cov.line_counts();
        let _ = writeln!(out, "  lines:     {hit}/{total}  ({})", percent(hit, total));
        // List up to a handful of uncovered lines (deterministic order).
        let uncovered: Vec<u32> = cov
            .lines_total
            .iter()
            .filter(|l| !cov.lines_hit.contains_key(l))
            .copied()
            .collect();
        if !uncovered.is_empty() {
            let shown: Vec<String> = uncovered
                .iter()
                .take(10)
                .map(|l| label_line(label, *l, smap))
                .collect();
            let more = if uncovered.len() > 10 {
                format!(", +{} more", uncovered.len() - 10)
            } else {
                String::new()
            };
            let _ = writeln!(out, "  uncovered lines: {}{more}", shown.join(", "));
        }
    }
    if sel != CoverageSel::Line {
        let hit = cov.fns_hit.len();
        let total = cov.fns_total;
        let _ = writeln!(out, "  functions: {hit}/{total}  ({})", percent(hit, total));
    }
}

/// Labels a (merged) line for the uncovered-line list: on the merged path, the
/// source map recovers the real `file:line`; otherwise (single-file, or a line that
/// maps to scaffolding) it falls back to `<label>:<line>`.
fn label_line(label: &str, line: u32, smap: Option<&multi::SourceMap>) -> String {
    if let Some(sm) = smap {
        if let Some((file, real)) = sm.resolve(line) {
            return format!("{file}:{real}");
        }
    }
    format!("{label}:{line}")
}

/// Formats `hit/total` as a one-decimal percentage using INTEGER math only (no
/// host float), so the output is byte-identical across runs and machines. A zero
/// denominator renders `100.0%` (vacuously fully covered).
fn percent(hit: usize, total: usize) -> String {
    if total == 0 {
        return "100.0%".to_string();
    }
    // tenths = round(hit*1000/total) in tenths of a percent.
    let tenths = (hit as u64 * 1000 + total as u64 / 2) / total as u64;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `k2c run --coverage <file>` support: runs `main` under coverage and prints the
/// same coverage section. Returns the program's exit code (coverage never changes
/// it). Compiled via the caller's already-lowered program.
pub fn run_with_coverage(
    prog: &k2_mir::MirProgram,
    args: RunArgs,
    label: &str,
    boundary: u32,
    sel_func_only: bool,
) -> ExitCode {
    // The VM applies the user/std boundary while draining coverage, so the reported
    // denominator is USER code only (std is appended after the user source on the
    // single-file run path) with honest per-(function, line) attribution.
    let (outcome, code, out, err, cov) = run_program_coverage(prog, args, boundary);
    let _ = io::stdout().write_all(&out);
    let _ = io::stdout().flush();
    let _ = io::stderr().write_all(&err);
    match outcome {
        RunOutcome::Ok => {}
        RunOutcome::Errored(name) => {
            let _ = writeln!(io::stderr(), "error: {name}");
        }
        RunOutcome::Panicked(msg) => {
            let _ = writeln!(io::stderr(), "panic: {msg}");
        }
    }
    let sel = if sel_func_only {
        CoverageSel::Func
    } else {
        CoverageSel::Both
    };
    // The `run --coverage` path is single-file (std appended after user source), so
    // the lines are already user coordinates — no source map relabeling needed.
    print_coverage(label, &cov, sel, None);
    ExitCode::from((code & 0xff) as u8)
}
