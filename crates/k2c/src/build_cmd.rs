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
    OptMode, OsInputs, RunArgs, TargetTriple,
};

use crate::lock;
use crate::multi::{self, CompileInputs, InputFiles};

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

/// Parses the `build` command line.
fn parse_args(args: &[String]) -> Result<BuildArgs, String> {
    let mut step: Option<String> = None;
    let mut build_file = PathBuf::from("build.k2");
    let mut optimize = OptMode::Debug;
    let mut target = TargetTriple::host();
    let mut dopts: Vec<(String, String)> = Vec::new();
    let mut forwarded: Vec<String> = Vec::new();
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
fn artifact_named_modules(
    graph: &BuildGraph,
    artifact: &Artifact,
    build_root: &Path,
) -> Vec<(String, PathBuf)> {
    let mut named_modules = Vec::new();
    for (name, mod_id) in &artifact.modules {
        if let Some(mod_artifact_id) = graph.module_artifact(*mod_id) {
            if let Some(mod_artifact) = graph.artifact(mod_artifact_id) {
                if let Some(mod_root) = &mod_artifact.root_source {
                    named_modules.push((name.clone(), build_root.join(mod_root)));
                }
            }
        }
    }
    named_modules
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
