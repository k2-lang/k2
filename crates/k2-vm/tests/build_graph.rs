//! End-to-end tests for the `*Build` capability floor: compile a `build.k2`
//! (with the bundled `build` module + `std` injected, exactly as the driver
//! does), run `build(b)` on the VM with `-D` inputs, and assert the recorded
//! [`BuildGraph`] — artifacts/options/steps/install in creation order — plus the
//! `run_tests` test runner.

use k2_mir::{lower_program, BuildMode};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;
use k2_vm::{run_build_graph, run_tests, ArtifactKind, BuildInputs, OptMode, TargetTriple};

/// Builds a one-`SourceFile` program from a `build.k2` body, injecting `std` and
/// the bundled `build` module the way the driver does (the std-injection move,
/// applied to `build`). `@import("build")` is left opaque (the `*Build` surface
/// is reached via value intrinsics), so only `std` is rewritten.
fn lower_build(body: &str) -> k2_mir::MirProgram {
    let mut combined = String::new();
    combined.push_str(body);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());
    combined.push_str(&k2_std::build_root_item_source());

    let pres = parse(&combined);
    assert!(pres.is_ok(), "parse errors: {:?}", pres.diagnostics);
    let resolved = resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve errors: {:?}",
        resolved.diagnostics
    );
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors: {:?}", typed.diagnostics);
    let prog = lower_program(&pres.file, &resolved, typed, BuildMode::Debug)
        .expect("lowering must succeed");
    assert!(prog.verify().is_empty(), "malformed MIR");
    prog
}

const BUILD_SRC: &str = r#"
const build = @import("build");
pub fn build(b: *Build) void {
    const target = b.standardTarget();
    const optimize = b.standardOptimize();
    const verbose = b.option(bool, "verbose", "verbose") orelse false;
    const lib = b.addLibrary(.{
        .name = "mylib",
        .root_source = b.path("lib.k2"),
        .target = target,
        .optimize = optimize,
    });
    lib.addOption(bool, "verbose", verbose);
    b.installArtifact(lib);
    const exe = b.addExecutable(.{
        .name = "myexe",
        .root_source = b.path("main.k2"),
        .target = target,
        .optimize = optimize,
    });
    exe.addModule("mylib", lib.module());
    b.installArtifact(exe);
    const run_step = b.step("run", "run it");
    const run_exe = b.addRunArtifact(exe);
    run_exe.passForwardedArgs();
    run_step.dependOn(&run_exe.step);
}
"#;

#[test]
fn records_artifacts_options_and_steps_in_order() {
    let prog = lower_build(BUILD_SRC);
    let graph = run_build_graph(
        &prog,
        BuildInputs {
            target: TargetTriple::host(),
            optimize: OptMode::Debug,
            dopts: vec![("verbose".to_string(), "true".to_string())],
            resolved_deps: Vec::new(),
        },
    )
    .expect("build(b) runs");

    // Creation order: lib(0), exe(1), run(2).
    assert_eq!(graph.artifacts.len(), 3);
    assert_eq!(graph.artifacts[0].kind, ArtifactKind::Library);
    assert_eq!(graph.artifacts[0].name, "mylib");
    assert_eq!(graph.artifacts[0].root_source.as_deref(), Some("lib.k2"));
    assert_eq!(graph.artifacts[1].kind, ArtifactKind::Executable);
    assert_eq!(graph.artifacts[1].name, "myexe");
    assert_eq!(graph.artifacts[2].kind, ArtifactKind::Run);
    assert_eq!(graph.artifacts[2].exe_id, Some(1));
    assert!(graph.artifacts[2].forward_args);

    // The library's `verbose` option was recorded from the `-D` map as a bool.
    assert_eq!(graph.artifacts[0].options.len(), 1);
    assert_eq!(graph.artifacts[0].options[0].0, "verbose");

    // The exe wired in the lib's module.
    assert_eq!(graph.artifacts[1].modules.len(), 1);
    assert_eq!(graph.artifacts[1].modules[0].0, "mylib");

    // install gathered lib + exe (creation order).
    assert_eq!(graph.install, vec![0, 1]);

    // The `run` named step depends on the run-artifact's embedded step.
    let run = graph.step_by_name("run").expect("run step");
    assert_eq!(run.deps, vec![graph.artifacts[2].step_id]);
}

#[test]
fn option_absent_returns_default() {
    let prog = lower_build(BUILD_SRC);
    // No `-Dverbose`, so `b.option(bool, "verbose", ...) orelse false` is false.
    let graph = run_build_graph(
        &prog,
        BuildInputs {
            target: TargetTriple::host(),
            optimize: OptMode::Debug,
            dopts: Vec::new(),
            resolved_deps: Vec::new(),
        },
    )
    .expect("build(b) runs");
    // The library's recorded option value is `false` (the default flowed through).
    match &graph.artifacts[0].options[0].1 {
        k2_vm::BuildOptVal::Bool(b) => assert!(!b),
        other => panic!("expected bool option, got {other:?}"),
    }
}

#[test]
fn target_and_optimize_surface_from_inputs() {
    let prog = lower_build(BUILD_SRC);
    let graph = run_build_graph(
        &prog,
        BuildInputs {
            target: TargetTriple::parse("aarch64-linux-musl"),
            optimize: OptMode::ReleaseFast,
            dopts: Vec::new(),
            resolved_deps: Vec::new(),
        },
    )
    .expect("build(b) runs");
    assert_eq!(graph.target.triple(), "aarch64-linux-musl");
    assert_eq!(graph.optimize, OptMode::ReleaseFast);
}

#[test]
fn run_tests_reports_pass_and_fail() {
    // A two-test program: one passes (returns void), one fails (returns an error
    // via `try` on an error value). `run_tests` runs each on a fresh fiber and
    // counts them. Written with plain `try` + a helper so the test needs no std
    // injection (the `std.testing.*` floor is exercised by the CLI `build test`).
    let src = r#"
fn ok() !void {
    return;
}
fn boom() !void {
    return error.Boom;
}
test "passes" {
    try ok();
}
test "fails" {
    try boom();
}
"#;
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
    let prog = lower_program(&pres.file, &resolved, typed, BuildMode::Debug).unwrap();

    let report = run_tests(&prog);
    assert_eq!(report.passed, 1, "lines: {:?}", report.lines);
    assert_eq!(report.failed, 1, "lines: {:?}", report.lines);
}
