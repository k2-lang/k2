//! # k2-vm — the bytecode compiler, register VM, and runtime shim (v0.8)
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the **execution layer (v0.8)** of the k2 front-end: it makes k2
//! programs *run*. It consumes the monomorphized, backend-agnostic
//! [`MirProgram`] produced by [`k2_mir`], compiles each
//! function to a compact register ISA, and executes `main(sys)` on a register VM
//! with a managed heap and a minimal capability runtime (the io/heap intrinsics).
//!
//! The pieces, in dependency order:
//!
//! 1. **The value model** (`value`) — a tagged [`Value`] where integers carry
//!    their width/signedness, aggregates are reference-counted for cheap
//!    copy-on-read, and pointers/slices are handles into the managed
//!    `heap`. Native memory layout is *not* modeled (that is post-0.13).
//! 2. **The ISA + compiler** (`isa`, `compile`) — MIR lowers to a flat,
//!    block-addressed instruction stream with a per-function constant pool;
//!    registers map 1:1 to MIR locals, so no register allocation is needed.
//! 3. **The VM** (`vm`) — an iterative-across-calls dispatch loop with a call
//!    stack of frames. A failed safety check, a `Trap`, or an `Unreachable`
//!    terminator becomes a clean runtime panic (a `panic:` line on stderr + a
//!    nonzero exit), **never** an uncontrolled Rust panic or abort. An
//!    instruction budget guarantees termination.
//! 4. **The runtime shim + intrinsics** (`vm`, `fmt`) — constructs the root
//!    `*System` capability, dispatches `sys.io.stdout()`/`stderr()`/`sys.heap`,
//!    formats `Writer.print`, and backs `create`/`destroy`/`alloc`/`free` with
//!    the managed heap. If `main` returns an error, the shim prints the error
//!    *name* to stderr and exits nonzero.
//!
//! ## Entry points
//!
//! [`run_program`] compiles + executes `main`, writing the program's stdout/
//! stderr to the real streams and returning the process [`ExitCode`]. The test
//! suite drives [`run_captured`], which captures stdout/stderr into buffers and
//! returns the structured [`RunOutcome`] plus the exit code, so it can assert
//! exact bytes without spawning a process.

mod build_graph;
mod compile;
mod fmt;
mod heap;
mod isa;
mod sched;
pub mod trace;
mod value;
mod vm;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::process::ExitCode;

use k2_mir::{BuildMode, MirProgram};

pub use build_graph::{
    Artifact, ArtifactKind, BuildGraph, BuildOptVal, DeclaredOption, DependencyNode, ModuleNode,
    OptMode, StepNode, TargetTriple,
};
pub use value::{Capability, IntRepr, SchedKind, Value};
pub use vm::{
    BuildInputs, Coverage, FailKind, Halt, OsInputs, PanicInfo, ResolvedDepSeed, RunOpts,
    TestFailure, TestOutcome, TestStatus, Vm,
};

/// Runs `build(b)` over a compiled `build.k2` program and returns the recorded
/// [`BuildGraph`]. The program is the merged `build.k2` (with the bundled `build`
/// module injected); the VM runs `build(b)` with a `*Build` capability backed by
/// recording intrinsics, then this returns the graph for the driver to execute.
///
/// A final `catch_unwind` backstop maps any stray internal Rust panic to a clean
/// [`Halt::Panic`] rather than aborting the process.
pub fn run_build_graph(prog: &MirProgram, inputs: BuildInputs) -> Result<BuildGraph, Halt> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut vm = Vm::new(prog);
        vm.run_build(inputs)
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err(Halt::Panic(PanicInfo::internal("build graph recording"))),
    }
}

/// The outcome of running a program's `test { ... }` blocks: how many passed and
/// failed, plus a per-test report line for each.
pub struct TestReport {
    /// The number of tests that passed.
    pub passed: usize,
    /// The number of tests that failed.
    pub failed: usize,
    /// One `name ... ok|FAILED (...)` line per test, in lowering order.
    pub lines: Vec<String>,
    /// The program's captured stdout across all tests.
    pub stdout: Vec<u8>,
    /// The program's captured stderr across all tests.
    pub stderr: Vec<u8>,
}

/// One test's structured result: its display name (the part after `test `), its
/// pass/fail status, and — on failure — a [`TestFailure`] carrying the reason and
/// the source span the runner renders a caret over.
#[derive(Clone, Debug)]
pub struct TestResult {
    /// The test's display name (the `"…"` part of `test "…" { }`).
    pub name: String,
    /// `true` iff the test passed (no failed assertion, trap, leak, or escaped
    /// error).
    pub passed: bool,
    /// The structured failure (reason + message + span), present iff `!passed`.
    pub failure: Option<TestFailure>,
    /// For a fuzz target, the number of iterations run before the result (`None`
    /// for a plain unit test).
    pub fuzz_runs: Option<usize>,
}

/// The rich outcome of running a program's `test`/`fuzz` blocks: the structured
/// per-test results, the captured streams, and (when requested) the coverage
/// summary. The driver renders FAILs with the v0.20 caret and prints the summary.
pub struct RichTestReport {
    /// The per-test results, in MIR lowering order (deterministic).
    pub results: Vec<TestResult>,
    /// The captured stdout across all tests.
    pub stdout: Vec<u8>,
    /// The captured stderr across all tests.
    pub stderr: Vec<u8>,
    /// The collected coverage, present iff `RunOpts::coverage` was set.
    pub coverage: Option<Coverage>,
}

impl RichTestReport {
    /// The number of tests that passed.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
    /// The number of tests that failed.
    pub fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.passed).count()
    }
}

/// Compiles + runs every `test { ... }` block in `prog`, returning a
/// [`TestReport`]. Each test runs on a fresh fiber with clean scheduler state, so
/// one test cannot leak fibers into the next. A `catch_unwind` backstop maps any
/// stray internal Rust panic to a failing report rather than aborting.
///
/// This is the legacy line-oriented entry point the build driver's `test` step
/// still consumes (it prints `# N passed, M failed`). The richer
/// [`run_tests_opts`] backs the first-class `k2c test` runner (caret diagnostics,
/// filter, coverage, fuzz). Both share the same per-test isolation + leak check.
pub fn run_tests(prog: &MirProgram) -> TestReport {
    let report = run_tests_opts(prog, &RunOpts::default());
    let lines: Vec<String> = report
        .results
        .iter()
        .map(|r| match &r.failure {
            None => format!("{} ... ok", display_test_name(&r.name)),
            Some(f) => format!("{} ... FAILED ({})", display_test_name(&r.name), f.message),
        })
        .collect();
    TestReport {
        passed: report.passed(),
        failed: report.failed(),
        lines,
        stdout: report.stdout,
        stderr: report.stderr,
    }
}

/// Renders a raw MIR test-function name (`"test foo"`) as its display form (the
/// part after the leading `test `), matching the legacy report wording.
fn display_test_name(raw: &str) -> &str {
    raw.strip_prefix("test ").unwrap_or(raw)
}

/// Compiles + runs the `test`/`fuzz` blocks of `prog` under `opts`, returning a
/// [`RichTestReport`]. Honours the name filter, collects coverage when asked, and
/// drives fuzz targets deterministically. A `catch_unwind` backstop maps any stray
/// internal Rust panic to a single synthetic failing result rather than aborting.
pub fn run_tests_opts(prog: &MirProgram, opts: &RunOpts) -> RichTestReport {
    let opts = opts.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut vm = Vm::new(prog);
        let (results, coverage) = vm.run_tests_opts(&opts);
        (results, coverage, vm.stdout, vm.stderr)
    }));
    match result {
        Ok((results, coverage, stdout, stderr)) => RichTestReport {
            results,
            stdout,
            stderr,
            coverage,
        },
        Err(_) => RichTestReport {
            results: vec![TestResult {
                name: "<internal>".to_string(),
                passed: false,
                failure: Some(TestFailure {
                    kind: FailKind::Trap,
                    message: "internal VM error (Rust panic) running tests".to_string(),
                    span: None,
                }),
                fuzz_runs: None,
            }],
            stdout: Vec::new(),
            stderr: Vec::new(),
            coverage: None,
        },
    }
}

/// Runs `main` under coverage instrumentation, returning the run outcome, exit
/// code, captured streams, and the collected [`Coverage`]. Backs `k2c run
/// --coverage`. `user_boundary` is the char offset at/after which a function/line
/// is appended std/prelude code excluded from the user denominator (`0` counts
/// everything). Deterministic: the VM is single-threaded and coverage uses ordered
/// `BTreeMap`/`BTreeSet`, so the report is identical across runs of the same MIR.
pub fn run_program_coverage(
    prog: &MirProgram,
    args: RunArgs,
    user_boundary: u32,
) -> (RunOutcome, i32, Vec<u8>, Vec<u8>, Coverage) {
    let mut os = args.os;
    if os.argv.is_empty() {
        os.argv = args.argv;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut vm = Vm::new(prog);
        vm.with_os_inputs(os);
        vm.set_coverage(true);
        let halt = vm.run_main();
        let (outcome, code) = match halt {
            Ok(()) => (RunOutcome::Ok, 0),
            Err(Halt::ProgramError(tag)) => {
                let name = prog
                    .err_names
                    .get(&k2_mir::ErrTag(tag))
                    .cloned()
                    .unwrap_or_else(|| format!("error{tag}"));
                (RunOutcome::Errored(name), 1)
            }
            Err(Halt::Panic(info)) => (RunOutcome::Panicked(info.message()), 134),
            Err(Halt::Exit(c)) => (RunOutcome::Ok, c),
        };
        // Exclude the std/prelude appended after the user source (single-file run
        // path) so the reported denominator is USER code only, with honest
        // per-(function, line) attribution.
        let cov = vm.coverage_with_boundary(user_boundary);
        (outcome, code, vm.stdout, vm.stderr, cov)
    }));
    match result {
        Ok(r) => r,
        Err(_) => (
            RunOutcome::Panicked("internal VM error (Rust panic)".to_string()),
            134,
            Vec::new(),
            Vec::new(),
            Coverage::default(),
        ),
    }
}

/// The arguments to a run: the build mode (informs ReleaseFast behaviour) and the
/// forwarded program argv (reserved; `main(sys)` takes no argv in v0.8).
pub struct RunArgs {
    /// The build mode the program was lowered under.
    pub mode: BuildMode,
    /// The program arguments (forwarded after `--`), read by `sys.os.args`/`arg`/
    /// `argCount`. Empty by default.
    pub argv: Vec<String>,
    /// The scripted environment map and real-env/real-pid opt-ins read by
    /// `sys.env.get` / `sys.os.getpid` (v0.23). All-default by default, so a run
    /// that never seeds it is offline-absent and deterministic.
    pub os: OsInputs,
    /// The file label used in an error-return trace (`<file>:line:col`). `None`
    /// falls back to a generic `<source>` label. Set by the driver from the
    /// source path so the trace points at real locations.
    pub trace_label: Option<String>,
}

impl RunArgs {
    /// The default run arguments for a given build mode.
    pub fn new(mode: BuildMode) -> RunArgs {
        RunArgs {
            mode,
            argv: Vec::new(),
            os: OsInputs::default(),
            trace_label: None,
        }
    }
}

/// The structured outcome of a captured run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// `main` completed successfully.
    Ok,
    /// `main` returned an error; the string is the error's name.
    Errored(String),
    /// The program panicked; the string is the panic message.
    Panicked(String),
}

/// Compiles and executes `main`, streaming the program's stdout/stderr to the
/// real process streams, and returns the process exit code. The VM never panics
/// the Rust process on a program error: a clean panic prints `panic: <message>`
/// to stderr and exits nonzero; an error escaping `main` prints `error: <name>`
/// and exits nonzero; success exits zero. A final `catch_unwind` backstop turns
/// any stray internal Rust panic into a nonzero exit rather than an abort.
pub fn run_program(prog: &MirProgram, args: RunArgs) -> ExitCode {
    exit_code(run_program_code(prog, args))
}

/// Like [`run_program`], but returns the raw `i32` exit code instead of an opaque
/// [`ExitCode`]. The streaming behaviour and the never-panic backstop are
/// identical; this entry point exists so a caller that must *aggregate* several
/// runs (the build driver's multi-artifact `run`/`test` steps) can inspect each
/// sub-run's success/failure and propagate the first nonzero code. `0` means
/// success; `134` is the internal-VM-error backstop.
pub fn run_program_code(prog: &MirProgram, args: RunArgs) -> i32 {
    let label = args
        .trace_label
        .clone()
        .unwrap_or_else(|| "<source>".to_string());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_inner_traced(prog, args)
    }));
    match result {
        Ok((outcome, code, out, err, trace)) => {
            // Flush the program's captured streams before any diagnostic line.
            let _ = std::io::stdout().write_all(&out);
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().write_all(&err);
            match outcome {
                RunOutcome::Ok => {}
                RunOutcome::Errored(name) => {
                    let _ = writeln!(std::io::stderr(), "error: {name}");
                    // In Debug/ReleaseSafe, print the error-return trace (the
                    // chain of `try` sites the error propagated through). It is
                    // empty in ReleaseFast, so nothing extra prints there.
                    let block = crate::trace::format_trace(&trace, &label);
                    if !block.is_empty() {
                        let _ = std::io::stderr().write_all(block.as_bytes());
                    }
                }
                RunOutcome::Panicked(msg) => {
                    let _ = writeln!(std::io::stderr(), "panic: {msg}");
                }
            }
            code
        }
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "panic: internal VM error (Rust panic)");
            134
        }
    }
}

/// Compiles and executes `main`, capturing stdout/stderr into buffers and
/// returning `(outcome, exit_code, stdout, stderr)`. Used by the test suite to
/// assert exact output and exit codes without spawning a process.
pub fn run_captured(prog: &MirProgram, args: RunArgs) -> (RunOutcome, i32, Vec<u8>, Vec<u8>) {
    // The forwarded argv and the scripted OS inputs (env / pid opt-ins) are threaded
    // into the VM. For the whole pre-v0.23 corpus these are all-default (empty argv,
    // offline-absent env, constant pid), so this is byte-identical to the previous
    // "ignore the args" behaviour; the v0.23 os/env/fs tests rely on the threading.
    let (outcome, code, out, err, _trace) = run_inner_traced(prog, args);
    (outcome, code, out, err)
}

/// Like [`run_captured`], but also returns the error-return trace (newest-first;
/// empty unless an error escaped `main` in Debug/ReleaseSafe). Used by tests that
/// assert on the propagation chain without spawning a process.
pub fn run_captured_traced(
    prog: &MirProgram,
    args: RunArgs,
) -> (
    RunOutcome,
    i32,
    Vec<u8>,
    Vec<u8>,
    Vec<crate::trace::TraceFrame>,
) {
    run_inner_traced(prog, args)
}

/// Like [`run_captured`], but also returns the number of VM instructions
/// executed — the deterministic, reproducible metric the benchmark harness uses
/// to compare Debug vs ReleaseFast. The VM is single-threaded and its dispatch is
/// data-deterministic, so this count is identical across runs of the same MIR.
///
/// The existing `run_captured`/`run_program` signatures are intentionally left
/// unchanged so no caller breaks; this is an additive entry point.
pub fn run_metered(prog: &MirProgram) -> (RunOutcome, i32, Vec<u8>, Vec<u8>, u64) {
    let (outcome, code, out, err, count, _trace) = run_inner_metered(prog, OsInputs::default());
    (outcome, code, out, err, count)
}

/// The shared core that also surfaces the error-return trace: builds a VM, runs
/// `main`, maps the [`Halt`], and returns the captured streams plus the
/// (possibly empty) propagation trace.
fn run_inner_traced(
    prog: &MirProgram,
    args: RunArgs,
) -> (
    RunOutcome,
    i32,
    Vec<u8>,
    Vec<u8>,
    Vec<crate::trace::TraceFrame>,
) {
    // Fold the forwarded argv into the OS inputs so `sys.os.args`/`arg`/`argCount`
    // (and the scripted env / pid opt-ins) reach the VM. Defaults are unchanged
    // (empty argv, offline-absent env), so a run with no `--`-args is byte-identical
    // to before.
    let mut os = args.os;
    if os.argv.is_empty() {
        os.argv = args.argv;
    }
    let (outcome, code, out, err, _count, trace) = run_inner_metered(prog, os);
    (outcome, code, out, err, trace)
}

/// The metered shared core: runs `main` and reports the outcome, exit code,
/// captured streams, executed-instruction count, and the error-return trace.
fn run_inner_metered(
    prog: &MirProgram,
    os: OsInputs,
) -> (
    RunOutcome,
    i32,
    Vec<u8>,
    Vec<u8>,
    u64,
    Vec<crate::trace::TraceFrame>,
) {
    let mut vm = Vm::new(prog);
    vm.with_os_inputs(os);
    let halt = vm.run_main();
    let (outcome, code) = match halt {
        Ok(()) => (RunOutcome::Ok, 0),
        Err(Halt::ProgramError(tag)) => {
            let name = prog
                .err_names
                .get(&k2_mir::ErrTag(tag))
                .cloned()
                .unwrap_or_else(|| format!("error{tag}"));
            (RunOutcome::Errored(name), 1)
        }
        Err(Halt::Panic(info)) => (RunOutcome::Panicked(info.message()), 134),
        // An explicit process exit, including an integer-returning `main` whose
        // result is the exit code. This is a *clean* termination, not an error, so
        // no diagnostic line is emitted — matching the native backend, which exits
        // with the code and prints nothing. The code passes straight through.
        Err(Halt::Exit(c)) => (RunOutcome::Ok, c),
    };
    let count = vm.instr_count();
    let trace = vm.escaped_trace().to_vec();
    (outcome, code, vm.stdout, vm.stderr, count, trace)
}

/// Maps an `i32` exit code to a process [`ExitCode`], clamping to a `u8`.
fn exit_code(code: i32) -> ExitCode {
    ExitCode::from((code & 0xff) as u8)
}
