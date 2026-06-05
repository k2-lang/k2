//! The `k2c build` subcommand: run `build(b)` to record the graph, then execute
//! the requested step.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! `k2c build [step] [-Dkey=value ...] [--build-file PATH] [-- prog-args...]`
//!
//! The build system IS k2: this command locates `build.k2`, compiles + runs its
//! `pub fn build(b: *Build)` on the VM with a `*Build` capability (the build-time
//! analogue of `*System`), which RECORDS a build graph via the `@build*`
//! recording intrinsics (no I/O during description — pure graph building). It
//! then reads the recorded graph and executes the requested step:
//!
//! * `install` / default — describes + validates the DAG (native artifact
//!   emission is a documented no-op until post-0.13 native codegen).
//! * `run` — builds + runs the chosen executable through the VM.
//! * `test` — compiles + runs the `test { ... }` blocks through the VM.
//!
//! A deterministic `build.lock` is written to the build root (reproducible: same
//! inputs → byte-identical lock). Nonzero exit on any error.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use k2_mir::BuildMode;
use k2_opt::{optimize, OptLevel};
use k2_vm::{
    run_build_graph, run_program_code, run_tests, Artifact, ArtifactKind, BuildGraph, BuildInputs,
    OptMode, OsInputs, ResolvedDepSeed, RunArgs, TargetTriple,
};

use crate::lock;
use crate::multi::{self, CompileInputs, InputFiles};
use crate::pkg::{self, ResolveConfig, ResolvedDeps};

/// Parsed `k2c build` command-line arguments.
struct BuildArgs {
    /// The requested step name (`install` by default).
    step: String,
    /// The `build.k2` path (default `./build.k2`).
    build_file: PathBuf,
    /// The resolved `-Doptimize` mode.
    optimize: OptMode,
    /// The resolved `-Dtarget` triple.
    target: TargetTriple,
    /// The remaining `-D` options (key -> value), deterministic order.
    dopts: Vec<(String, String)>,
    /// Program arguments forwarded after `--`.
    forwarded: Vec<String>,
    /// v0.25: an explicit `--registry <dir>` override for the local vendored
    /// registry root (highest-precedence registry config).
    registry: Option<PathBuf>,
    /// v0.25: `--update` re-resolves dependencies from scratch, ignoring any
    /// present `deps.lock`, and rewrites the lock.
    update: bool,
}

/// Entry point for `k2c build`.
pub fn cmd_build(args: &[String]) -> Result<ExitCode, String> {
    let parsed = parse_args(args)?;
    let build_file = &parsed.build_file;
    if !build_file.exists() {
        return Err(format!(
            "no build file at `{}` (pass --build-file PATH)",
            build_file.display()
        ));
    }
    // The build root is the build file's directory (spec §08).
    let build_root = build_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // --- 0. Pre-resolve dependencies (v0.25, the offline package manager). ----
    // Resolution is I/O (read the registry, parse manifests, hash bytes), which
    // the comptime sandbox forbids inside build(b) — so the driver does it FIRST,
    // honoring an existing `deps.lock` unless `--update`. The result seeds
    // BuildInputs.resolved_deps; the VM later mints synthetic dep libraries from
    // it. A missing/unsatisfiable/conflict/cycle is reported here (before any
    // compile) with a nonzero exit — never a silent wrong build.
    let resolved_deps = match resolve_dependencies(&build_root, &parsed) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io::stderr(), "build: {}", e.message);
            return Ok(ExitCode::FAILURE);
        }
    };
    let dep_seeds = dependency_seeds(&resolved_deps);

    // --- 1. Compile build.k2 (with the bundled `build` module injected) and run
    //        build(b) on the VM to RECORD the graph. ---------------------------
    let merged = multi::merge(&CompileInputs {
        root_source: build_file.clone(),
        build_root: build_root.clone(),
        named_modules: Vec::new(),
        build_options: Vec::new(),
        inject_build: true,
    })
    .map_err(|d| format!("{}: {}", d.label, d.message))?;

    let prog = match multi::compile_merged(&merged.source, "build.k2", BuildMode::Debug) {
        Ok(p) => p,
        Err(diags) => {
            let stderr = io::stderr();
            let mut err = stderr.lock();
            for d in &diags {
                let _ = writeln!(err, "build.k2: {}", d.message);
            }
            return Ok(ExitCode::FAILURE);
        }
    };

    let inputs = BuildInputs {
        target: parsed.target.clone(),
        optimize: parsed.optimize,
        dopts: parsed.dopts.clone(),
        resolved_deps: dep_seeds,
    };
    let graph = match run_build_graph(&prog, inputs) {
        Ok(g) => g,
        Err(h) => {
            let _ = writeln!(io::stderr(), "build: {}", halt_message(&h));
            return Ok(ExitCode::FAILURE);
        }
    };

    // --- 2. Write the deterministic lockfile (reproducible). -----------------
    // Fingerprint EVERY resolved `.k2` input — not just `build.k2`, but every
    // buildable artifact's `root_source` plus its transitive path imports and
    // wired named modules — so a change to ANY compiled source flips graph_hash
    // and makes drift visible (spec §08.7). The set is deduplicated and sorted in
    // `lock::serialize`, so identical inputs still yield a byte-identical lock.
    let all_inputs = collect_all_inputs(&graph, &build_root, &merged.inputs);
    let lock_text = lock::serialize(&graph, &all_inputs, &parsed.dopts);
    let lock_path = build_root.join("build.lock");
    let _ = lock::write_if_changed(&lock_path, &lock_text);

    // The v0.25 dependency lock: pins each resolved package@version + source +
    // content hash, in deterministic order. Identical manifest+registry yields a
    // byte-identical `deps.lock`. Only written when the project declares deps (a
    // no-dependency project never gets a `deps.lock`, keeping the existing
    // examples untouched).
    if !resolved_deps.deps.is_empty() {
        let deps_lock_text = lock::serialize_deps(&resolved_deps);
        let deps_lock_path = build_root.join("deps.lock");
        let _ = lock::write_if_changed(&deps_lock_path, &deps_lock_text);
    }

    // --- 3. Execute the requested step. --------------------------------------
    match parsed.step.as_str() {
        "install" | "build" => step_describe(&graph, &build_root),
        "run" => step_run(&graph, &build_root, parsed.optimize, &parsed.forwarded),
        "test" => step_test(&graph, &build_root, parsed.optimize),
        other => {
            // A user-registered step: resolve it and run its reachable artifacts.
            if let Some(step) = graph.step_by_name(other) {
                run_user_step(
                    &graph,
                    &build_root,
                    parsed.optimize,
                    step.id,
                    &parsed.forwarded,
                )
            } else {
                Err(format!("unknown build step `{other}`"))
            }
        }
    }
}

/// Entry point for `k2c update` (v0.25): re-resolve every declared dependency from
/// scratch (ignoring any present `deps.lock`), write a fresh `deps.lock`, and exit
/// WITHOUT compiling. A thin alias for the resolve+lock phase of `k2c build
/// --update`. Accepts `--build-file PATH` and `--registry DIR`.
pub fn cmd_update(args: &[String]) -> Result<ExitCode, String> {
    let mut parsed = parse_args(args)?;
    parsed.update = true; // `update` always re-resolves from scratch
    let build_file = &parsed.build_file;
    let build_root = build_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let resolved = match resolve_dependencies(&build_root, &parsed) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io::stderr(), "update: {}", e.message);
            return Ok(ExitCode::FAILURE);
        }
    };

    if resolved.deps.is_empty() {
        let _ = writeln!(
            io::stdout(),
            "# no dependencies declared (no k2.pkg, or an empty .dependencies)"
        );
        return Ok(ExitCode::SUCCESS);
    }

    let deps_lock_text = lock::serialize_deps(&resolved);
    let deps_lock_path = build_root.join("deps.lock");
    let _ = lock::write_if_changed(&deps_lock_path, &deps_lock_text);

    let mut out = io::stdout();
    let _ = writeln!(out, "# resolved {} package(s)", resolved.deps.len());
    for d in &resolved.deps {
        let _ = writeln!(out, "  {} {} ({})", d.name, d.version, d.source_desc);
    }
    Ok(ExitCode::SUCCESS)
}

/// Parses the `build` command line.
fn parse_args(args: &[String]) -> Result<BuildArgs, String> {
    let mut step: Option<String> = None;
    let mut build_file = PathBuf::from("build.k2");
    let mut optimize = OptMode::Debug;
    let mut target = TargetTriple::host();
    let mut dopts: Vec<(String, String)> = Vec::new();
    let mut forwarded: Vec<String> = Vec::new();
    let mut registry: Option<PathBuf> = None;
    let mut update = false;
    let mut after_dashdash = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if after_dashdash {
            forwarded.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
            continue;
        }
        if arg == "--build-file" {
            let p = it
                .next()
                .ok_or_else(|| "`--build-file` needs a path".to_string())?;
            build_file = PathBuf::from(p);
            continue;
        }
        if arg == "--registry" {
            let p = it
                .next()
                .ok_or_else(|| "`--registry` needs a directory path".to_string())?;
            registry = Some(PathBuf::from(p));
            continue;
        }
        if arg == "--update" {
            update = true;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("-D") {
            let (key, val) = match rest.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                // A bare `-Dkey` is a true boolean flag.
                None => (rest.to_string(), "true".to_string()),
            };
            match key.as_str() {
                "optimize" => optimize = OptMode::parse(&val),
                "target" => target = TargetTriple::parse(&val),
                _ => {
                    // Last write wins; keep insertion order otherwise.
                    if let Some(slot) = dopts.iter_mut().find(|(k, _)| *k == key) {
                        slot.1 = val;
                    } else {
                        dopts.push((key, val));
                    }
                }
            }
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("unknown `build` flag `{arg}`"));
        }
        if step.is_some() {
            return Err(format!("`build` takes at most one step; got extra `{arg}`"));
        }
        step = Some(arg.clone());
    }

    // Sort dopts so the lock and the VM see a deterministic order.
    dopts.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(BuildArgs {
        step: step.unwrap_or_else(|| "install".to_string()),
        build_file,
        optimize,
        target,
        dopts,
        forwarded,
        registry,
        update,
    })
}

/// The `install` / default step: describe + validate the DAG. Every artifact's
/// `root_source` is compiled (so a broken module fails the build); native
/// emission is a documented no-op on the VM toolchain.
fn step_describe(graph: &BuildGraph, build_root: &Path) -> Result<ExitCode, String> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "# k2 build graph");
    let _ = writeln!(
        out,
        "# target={} optimize={}",
        graph.target.triple(),
        graph.optimize.name()
    );
    let _ = writeln!(
        out,
        "# (native emission is a no-op on the VM toolchain; artifacts are validated, not written)"
    );

    if !graph.options.is_empty() {
        let _ = writeln!(out, "options:");
        let mut opts: Vec<&k2_vm::DeclaredOption> = graph.options.iter().collect();
        opts.sort_by(|a, b| a.name.cmp(&b.name));
        for o in opts {
            let _ = writeln!(out, "  -D{}={} : {}", o.name, o.kind, o.desc);
        }
    }

    let _ = writeln!(out, "artifacts:");
    let mut compile_errors = 0usize;
    for a in &graph.artifacts {
        describe_artifact(&mut out, graph, a);
        // Validate buildable artifacts (lib/exe/test) by compiling them.
        if matches!(
            a.kind,
            ArtifactKind::Library | ArtifactKind::Executable | ArtifactKind::Test
        ) {
            if let Some(root) = &a.root_source {
                let mode = mode_for(graph.optimize);
                match compile_artifact(graph, a, build_root, root, mode, false) {
                    Ok(_) => {
                        let _ = writeln!(out, "    [ok] {} compiles", a.name);
                    }
                    Err(msg) => {
                        compile_errors += 1;
                        let _ = writeln!(out, "    [FAIL] {}: {}", a.name, msg);
                    }
                }
            }
        }
    }

    let _ = writeln!(out, "steps:");
    let mut steps: Vec<&k2_vm::StepNode> =
        graph.steps.iter().filter(|s| s.name.is_some()).collect();
    steps.sort_by(|a, b| a.name.cmp(&b.name));
    let _ = writeln!(
        out,
        "  install (default) -> artifacts [{}]",
        join_ids(&graph.install)
    );
    for s in steps {
        let _ = writeln!(
            out,
            "  {} : {} (deps: {})",
            s.name.as_deref().unwrap_or(""),
            s.desc,
            join_ids(&s.deps)
        );
    }

    if compile_errors > 0 {
        let _ = writeln!(
            io::stderr(),
            "build: {compile_errors} artifact(s) failed to compile"
        );
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Prints one artifact's description line(s).
fn describe_artifact(out: &mut impl Write, graph: &BuildGraph, a: &Artifact) {
    let root = a.root_source.as_deref().unwrap_or("-");
    let _ = writeln!(
        out,
        "  [{}] {} (id={}) root={}",
        a.kind.keyword(),
        a.name,
        a.id,
        root
    );
    for (name, mod_id) in &a.modules {
        let target = graph
            .module_artifact(*mod_id)
            .and_then(|aid| graph.artifact(aid))
            .map(|m| m.root_source.clone().unwrap_or_default())
            .unwrap_or_default();
        let _ = writeln!(out, "      module {name} -> {target}");
    }
    for (name, val) in &a.options {
        let _ = writeln!(out, "      option {name} = {}", val.display());
    }
    if let Some(exe) = a.exe_id {
        let target = graph
            .artifact(exe)
            .map(|e| e.name.clone())
            .unwrap_or_default();
        let _ = writeln!(out, "      runs {target}");
    }
}

/// The `run` step: find EVERY `Run` artifact reachable from the `run` step,
/// compile each wrapped executable as a multi-file program, and run them all
/// through the VM in declaration order. A step that `dependOn`s several
/// run-artifacts runs all of them; the step fails if any sub-run exits nonzero
/// (the first nonzero code is propagated).
fn step_run(
    graph: &BuildGraph,
    build_root: &Path,
    optimize: OptMode,
    forwarded: &[String],
) -> Result<ExitCode, String> {
    let run_step = graph
        .step_by_name("run")
        .ok_or_else(|| "no `run` step is registered in build.k2".to_string())?;
    let run_artifacts = reachable_run_artifacts(graph, run_step.id);
    if run_artifacts.is_empty() {
        return Err("the `run` step has no run-artifact".to_string());
    }
    let mode = mode_for(optimize);
    let mut first_failure: i32 = 0;
    for run_artifact in run_artifacts {
        let exe_id = run_artifact
            .exe_id
            .ok_or_else(|| "run-artifact has no executable".to_string())?;
        let exe = graph
            .artifact(exe_id)
            .ok_or_else(|| "run-artifact targets a missing executable".to_string())?;
        let root = exe
            .root_source
            .as_ref()
            .ok_or_else(|| format!("executable `{}` has no root_source", exe.name))?;
        let prog = compile_artifact(graph, exe, build_root, root, mode, false)
            .map_err(|m| format!("compiling `{}`: {m}", exe.name))?;
        let argv = if run_artifact.forward_args {
            forwarded.to_vec()
        } else {
            Vec::new()
        };
        let code = run_program_code(
            &prog,
            RunArgs {
                mode,
                argv,
                os: OsInputs::default(),
                trace_label: None,
            },
        );
        // The FIRST nonzero sub-run fails the whole step; remaining runs still
        // execute so their output is never silently dropped.
        if code != 0 && first_failure == 0 {
            first_failure = code;
        }
    }
    Ok(exit_code_from(first_failure))
}

/// The `test` step: find EVERY `Run` artifact over a `Test` artifact reachable
/// from the `test` step, compile each with tests, and run every `test { ... }`
/// block through the VM — in declaration order. A `test` step that `dependOn`s
/// several test suites runs ALL of them; the per-suite reports are AGGREGATED
/// (passed/failed summed across suites) and the step fails if any suite has a
/// failing test. This is the spec's documented pattern (§08, wiring `run_unit`
/// AND `run_integ` into one `test` step), which previously ran only one suite.
fn step_test(graph: &BuildGraph, build_root: &Path, optimize: OptMode) -> Result<ExitCode, String> {
    let test_step = graph
        .step_by_name("test")
        .ok_or_else(|| "no `test` step is registered in build.k2".to_string())?;
    // The test step depends on one or more run-artifacts over Test artifacts.
    let run_artifacts = reachable_run_artifacts(graph, test_step.id);
    if run_artifacts.is_empty() {
        return Err("the `test` step has no run-artifact".to_string());
    }
    let mode = mode_for(optimize);
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    for run_artifact in run_artifacts {
        let test_id = run_artifact
            .exe_id
            .ok_or_else(|| "test run-artifact has no test artifact".to_string())?;
        let test = graph
            .artifact(test_id)
            .ok_or_else(|| "test run-artifact targets a missing test".to_string())?;
        let root = test
            .root_source
            .as_ref()
            .ok_or_else(|| format!("test `{}` has no root_source", test.name))?;
        let prog = compile_artifact(graph, test, build_root, root, mode, true)
            .map_err(|m| format!("compiling test `{}`: {m}", test.name))?;

        let report = run_tests(&prog);
        // Stream this suite's own output, then its per-test report.
        let _ = io::stdout().write_all(&report.stdout);
        let _ = err.write_all(&report.stderr);
        for line in &report.lines {
            let _ = writeln!(err, "test {line}");
        }
        total_passed += report.passed;
        total_failed += report.failed;
    }
    let _ = writeln!(err, "# {total_passed} passed, {total_failed} failed");
    if total_failed == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// Runs a user-registered step: compile + run EVERY run-artifact reachable from
/// it (executables and tests), in declaration order. Executables run on the VM;
/// test artifacts run their `test { ... }` blocks. The step fails if any
/// sub-artifact fails (the first nonzero executable code, or any failing test).
/// A no-op step (no reachable run-artifact) is a successful describe of its deps.
fn run_user_step(
    graph: &BuildGraph,
    build_root: &Path,
    optimize: OptMode,
    step_id: u32,
    forwarded: &[String],
) -> Result<ExitCode, String> {
    let run_artifacts = reachable_run_artifacts(graph, step_id);
    if run_artifacts.is_empty() {
        return step_describe(graph, build_root);
    }
    let mode = mode_for(optimize);
    let mut first_failure: i32 = 0;
    let mut any_test_failed = false;
    for run_artifact in run_artifacts {
        let exe_id = run_artifact
            .exe_id
            .ok_or_else(|| "step run-artifact has no target".to_string())?;
        let exe = graph
            .artifact(exe_id)
            .ok_or_else(|| "step targets a missing artifact".to_string())?;
        let root = exe
            .root_source
            .as_ref()
            .ok_or_else(|| format!("`{}` has no root_source", exe.name))?;
        let prog = compile_artifact(
            graph,
            exe,
            build_root,
            root,
            mode,
            exe.kind == ArtifactKind::Test,
        )
        .map_err(|m| format!("compiling `{}`: {m}", exe.name))?;
        if exe.kind == ArtifactKind::Test {
            let report = run_tests(&prog);
            let _ = io::stdout().write_all(&report.stdout);
            let stderr = io::stderr();
            let mut err = stderr.lock();
            let _ = err.write_all(&report.stderr);
            for line in &report.lines {
                let _ = writeln!(err, "test {line}");
            }
            if report.failed != 0 {
                any_test_failed = true;
            }
            continue;
        }
        let argv = if run_artifact.forward_args {
            forwarded.to_vec()
        } else {
            Vec::new()
        };
        let code = run_program_code(
            &prog,
            RunArgs {
                mode,
                argv,
                os: OsInputs::default(),
                trace_label: None,
            },
        );
        if code != 0 && first_failure == 0 {
            first_failure = code;
        }
    }
    if any_test_failed && first_failure == 0 {
        first_failure = 1;
    }
    Ok(exit_code_from(first_failure))
}

/// Collects EVERY `Run` artifact reachable from `step_id` by walking the step
/// dependency graph in **declaration order** — a recursive post-order, left-to-
/// right over each step's `deps`, so a `test`/user step that `dependOn`s several
/// run-artifacts executes ALL of them, in the order they were wired (the
/// FIRST-declared dependency first). A single `reachable_run_artifact` that
/// returned one LIFO artifact silently dropped every other suite/executable
/// (`k2 build test` could skip suites while still exiting 0); collecting them all
/// fixes that.
///
/// The walk is deterministic and cycle-safe (`seen` guards re-entry). It is a
/// post-order so that a dependency's own sub-dependencies run before it, matching
/// the spec's `dependOn` ordering semantics. Duplicate artifacts (reached by two
/// paths) are emitted once, at their first (deepest, left-most) occurrence.
fn reachable_run_artifacts(graph: &BuildGraph, step_id: u32) -> Vec<&Artifact> {
    let mut seen_steps = std::collections::BTreeSet::new();
    let mut emitted = std::collections::BTreeSet::new();
    let mut out: Vec<&Artifact> = Vec::new();
    collect_run_artifacts(graph, step_id, &mut seen_steps, &mut emitted, &mut out);
    out
}

/// The recursive post-order helper for [`reachable_run_artifacts`]: visit each
/// dependency (left-to-right) before recording this step's own `Run` artifact, so
/// declaration order is preserved and the deepest dependency runs first.
fn collect_run_artifacts<'g>(
    graph: &'g BuildGraph,
    step_id: u32,
    seen_steps: &mut std::collections::BTreeSet<u32>,
    emitted: &mut std::collections::BTreeSet<u32>,
    out: &mut Vec<&'g Artifact>,
) {
    if !seen_steps.insert(step_id) {
        return;
    }
    // Visit dependencies first, in declared (left-to-right) order.
    if let Some(step) = graph.step(step_id) {
        for &dep in &step.deps {
            collect_run_artifacts(graph, dep, seen_steps, emitted, out);
        }
    }
    // Then record this step's own embedded `Run` artifact, if it has one.
    if let Some(a) = graph.artifacts.iter().find(|a| a.step_id == step_id) {
        if a.kind == ArtifactKind::Run && emitted.insert(a.id) {
            out.push(a);
        }
    }
}

/// Resolves an artifact's wired named modules (`addModule`) to their on-disk root
/// files: `(import_name, absolute_path)` in insertion order.
///
/// This is TRANSITIVE: a wired module's OWN named modules are included too, so a
/// dependency's `@import("child")` resolves when the consumer only wired the
/// parent. The merge's `named` map is global (it rewrites `@import` in EVERY
/// merged file), so threading the whole transitive closure here lets a registry
/// package's `@import("baz")` rewrite to `baz`'s namespace even though the
/// top-level artifact never named `baz`. Cycle-safe (a `seen` set guards a
/// dependency that re-imports an ancestor); a name first wired by the consumer
/// wins over a deeper re-binding (the consumer's wiring is authoritative).
fn artifact_named_modules(
    graph: &BuildGraph,
    artifact: &Artifact,
    build_root: &Path,
) -> Vec<(String, PathBuf)> {
    let mut named_modules: Vec<(String, PathBuf)> = Vec::new();
    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut seen_artifacts: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    collect_named_modules(
        graph,
        artifact,
        build_root,
        &mut named_modules,
        &mut seen_names,
        &mut seen_artifacts,
    );
    named_modules
}

/// The recursive helper for [`artifact_named_modules`]: appends `artifact`'s wired
/// modules, then recurses into each wired module's defining library so transitive
/// named imports (a dependency's deps) are surfaced into the same global merge map.
fn collect_named_modules(
    graph: &BuildGraph,
    artifact: &Artifact,
    build_root: &Path,
    out: &mut Vec<(String, PathBuf)>,
    seen_names: &mut std::collections::BTreeSet<String>,
    seen_artifacts: &mut std::collections::BTreeSet<u32>,
) {
    if !seen_artifacts.insert(artifact.id) {
        return;
    }
    for (name, mod_id) in &artifact.modules {
        let Some(mod_artifact_id) = graph.module_artifact(*mod_id) else {
            continue;
        };
        let Some(mod_artifact) = graph.artifact(mod_artifact_id) else {
            continue;
        };
        if let Some(mod_root) = &mod_artifact.root_source {
            // The consumer's own wiring is authoritative: a name already bound is
            // not overridden by a deeper (transitive) binding of the same name.
            if seen_names.insert(name.clone()) {
                out.push((name.clone(), build_root.join(mod_root)));
            }
        }
        // Recurse into the wired library to surface ITS named modules too.
        collect_named_modules(
            graph,
            mod_artifact,
            build_root,
            out,
            seen_names,
            seen_artifacts,
        );
    }
}

/// Unions the resolved `.k2` input set of `build.k2` with that of EVERY buildable
/// artifact (library/executable/test) — each artifact's `root_source` plus its
/// transitive path imports and wired named modules — so the lock fingerprints all
/// sources, not just `build.k2`. A merge failure for one artifact is non-fatal
/// here (the artifact still compiles/fails in its own step with a real
/// diagnostic); the lock simply records whatever inputs resolved. The result is
/// deduplicated by relative path (first occurrence wins) and re-sorted in
/// `lock::serialize`.
fn collect_all_inputs(
    graph: &BuildGraph,
    build_root: &Path,
    build_inputs: &InputFiles,
) -> InputFiles {
    let mut files: Vec<(String, PathBuf)> = build_inputs.files.clone();
    for a in &graph.artifacts {
        if !matches!(
            a.kind,
            ArtifactKind::Library | ArtifactKind::Executable | ArtifactKind::Test
        ) {
            continue;
        }
        let Some(root_source) = &a.root_source else {
            continue;
        };
        let named_modules = artifact_named_modules(graph, a, build_root);
        let merged = multi::merge(&CompileInputs {
            root_source: build_root.join(root_source),
            build_root: build_root.to_path_buf(),
            named_modules,
            build_options: a.options.clone(),
            inject_build: false,
        });
        if let Ok(merged) = merged {
            for (rel, abs) in merged.inputs.files {
                // Deduplicate by absolute path so two distinct files that share a
                // basename (same `rel` fallback) are both fingerprinted.
                if !files.iter().any(|(_, a)| *a == abs) {
                    files.push((rel, abs));
                }
            }
        }
    }
    InputFiles { files }
}

/// Compiles one artifact as a multi-file program: resolves its `root_source` +
/// wired modules + build options into a merged program, lowers + optimizes it.
/// `_with_tests` is reserved (the lowering already emits every `test` block; the
/// VM's `run_tests` selects them).
fn compile_artifact(
    graph: &BuildGraph,
    artifact: &Artifact,
    build_root: &Path,
    root_source: &str,
    mode: BuildMode,
    _with_tests: bool,
) -> Result<k2_mir::MirProgram, String> {
    let root_path = build_root.join(root_source);
    // Resolve the artifact's wired named modules to their root files.
    let named_modules = artifact_named_modules(graph, artifact, build_root);
    let merged = multi::merge(&CompileInputs {
        root_source: root_path,
        build_root: build_root.to_path_buf(),
        named_modules,
        build_options: artifact.options.clone(),
        inject_build: false,
    })
    .map_err(|d| format!("{}: {}", d.label, d.message))?;

    let mut prog = multi::compile_merged(&merged.source, root_source, mode).map_err(|diags| {
        diags
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    // Apply the optimizer per the mode (ReleaseSafe/ReleaseFast optimize; Debug
    // is left unoptimized so checks stay intact).
    let level = match mode {
        BuildMode::Debug => OptLevel::None,
        BuildMode::ReleaseSafe => OptLevel::Safe,
        BuildMode::ReleaseFast => OptLevel::Fast,
    };
    optimize(&mut prog, level);
    let problems = prog.verify();
    if !problems.is_empty() {
        return Err(format!(
            "malformed MIR after optimize: {}",
            problems[0].message
        ));
    }
    Ok(prog)
}

/// Pass A of the v0.25 package manager: read the project `k2.pkg` (if any),
/// resolve every declared dependency offline (path + registry/semver), honoring an
/// existing `deps.lock` unless `--update`. Returns the resolved table (empty if the
/// project declares no dependencies). All resolution errors (missing/unsatisfiable/
/// conflict/cycle/malformed) surface here as a clear [`pkg::ResolveError`].
fn resolve_dependencies(
    build_root: &Path,
    parsed: &BuildArgs,
) -> Result<ResolvedDeps, pkg::ResolveError> {
    let manifest_path = build_root.join("k2.pkg");
    // No project manifest ⇒ no dependencies (the existing no-dep path).
    if !manifest_path.exists() {
        return Ok(ResolvedDeps::default());
    }
    let manifest = pkg::read_manifest(&manifest_path)?;
    if manifest.dependencies.is_empty() {
        return Ok(ResolvedDeps::default());
    }

    let (registry_root, registry_display) = registry_config(build_root, &manifest, parsed);

    // Load a present `deps.lock` (unless --update) so the resolver PINS to the
    // locked versions where they still satisfy the manifest — an ordinary build
    // never silently moves a dependency (spec §7.3). `--update` ignores the lock
    // and re-resolves to the newest matching versions.
    let lock_path = build_root.join("deps.lock");
    let parsed_lock = if parsed.update {
        lock::ParsedDepsLock::default()
    } else {
        std::fs::read_to_string(&lock_path)
            .map(|t| lock::parse_deps(&t))
            .unwrap_or_default()
    };
    let locked: std::collections::BTreeMap<String, String> = parsed_lock
        .deps
        .iter()
        .map(|(name, d)| (name.clone(), d.version.clone()))
        .collect();

    let config = ResolveConfig {
        registry_root,
        registry_display,
        locked,
    };

    let resolved = pkg::resolve_project(&manifest, build_root, &config)?;
    // With the lock honored, the pinned version is already chosen; a content-hash
    // mismatch on a locked package (its bytes changed underneath the lock) is a
    // clear "lock out of date" error rather than a silent rebuild.
    check_lock_drift(&parsed_lock, &resolved)?;
    Ok(resolved)
}

/// Verifies that each freshly-resolved dependency whose version matches the
/// present `deps.lock` still has the locked content hash. A content-hash mismatch
/// (the package bytes changed while pinned) is a clear "lock out of date"
/// diagnostic (run `k2c update`) rather than a silent rebuild against changed
/// bytes. A version DIFFERENCE is not flagged here — the resolver already pins to
/// the locked version when it remains valid, so a difference means the lock was
/// intentionally re-resolved (`--update`) or the locked version is gone.
fn check_lock_drift(
    locked: &lock::ParsedDepsLock,
    resolved: &ResolvedDeps,
) -> Result<(), pkg::ResolveError> {
    for dep in &resolved.deps {
        if let Some(l) = locked.get(&dep.name) {
            let cur_version = dep.version.to_string();
            // Only compare hashes when the version is the locked one (a re-resolve
            // to a different version legitimately has a different hash).
            if l.version == cur_version && !l.hash.is_empty() && l.hash != dep.hash {
                return Err(pkg::ResolveError {
                    message: format!(
                        "deps.lock out of date: {}@{} content changed (hash mismatch); run `k2c update`",
                        dep.name, cur_version
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Resolves the registry root + its display string with the precedence
/// `--registry` > `K2_REGISTRY` env > project `k2.pkg` `.registry` > default
/// `<build_root>/vendor`. The display is a short, lock-friendly string (the
/// configured spelling, not the absolute path) for a reproducible header.
fn registry_config(
    build_root: &Path,
    manifest: &pkg::Manifest,
    parsed: &BuildArgs,
) -> (PathBuf, String) {
    if let Some(reg) = &parsed.registry {
        return (reg.clone(), reg.display().to_string());
    }
    if let Ok(env) = std::env::var("K2_REGISTRY") {
        if !env.is_empty() {
            return (PathBuf::from(&env), env);
        }
    }
    if let Some(reg) = &manifest.registry {
        return (build_root.join(reg), reg.clone());
    }
    (build_root.join("vendor"), "vendor".to_string())
}

/// Flattens the resolved dependency table into the VM seeds the `*Build` floor
/// reads (v0.25): each declared dependency's resolved root path plus its OWN
/// children's `(name, root)` pairs, so a synthetic library can wire them as named
/// modules. EVERY resolved package is seeded (not just the top-level ones), so a
/// transitive child reached only through another dep is mintable.
fn dependency_seeds(resolved: &ResolvedDeps) -> Vec<ResolvedDepSeed> {
    let mut seeds: Vec<ResolvedDepSeed> = Vec::new();
    for dep in &resolved.deps {
        let mut children: Vec<(String, String)> = Vec::new();
        for child_name in &dep.children {
            if let Some(child) = resolved.get(child_name) {
                children.push((
                    child_name.clone(),
                    child.root_abs.to_string_lossy().into_owned(),
                ));
            }
        }
        children.sort();
        seeds.push(ResolvedDepSeed {
            name: dep.name.clone(),
            root_abs: dep.root_abs.to_string_lossy().into_owned(),
            children,
        });
    }
    seeds
}

/// Maps a raw `i32` exit code to a process [`ExitCode`], clamped to a `u8` (the
/// process-exit width); `0` is success.
fn exit_code_from(code: i32) -> ExitCode {
    ExitCode::from((code & 0xff) as u8)
}

/// Maps an [`OptMode`] to the MIR build mode the artifact lowers under.
fn mode_for(opt: OptMode) -> BuildMode {
    match opt {
        OptMode::Debug => BuildMode::Debug,
        OptMode::ReleaseSafe => BuildMode::ReleaseSafe,
        OptMode::ReleaseFast => BuildMode::ReleaseFast,
    }
}

/// Renders a comma-separated id list.
fn join_ids(ids: &[u32]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A human message for a build-time [`Halt`].
fn halt_message(h: &k2_vm::Halt) -> String {
    match h {
        k2_vm::Halt::Panic(info) => format!("build script panicked: {}", info.message()),
        k2_vm::Halt::ProgramError(tag) => format!("build script returned error tag {tag}"),
        k2_vm::Halt::Exit(c) => format!("build script exited with {c}"),
    }
}
