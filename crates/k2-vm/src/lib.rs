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
mod value;
mod vm;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::process::ExitCode;

use k2_mir::{BuildMode, MirProgram};

pub use build_graph::{
    Artifact, ArtifactKind, BuildGraph, BuildOptVal, DeclaredOption, ModuleNode, OptMode, StepNode,
    TargetTriple,
};
pub use value::{Capability, IntRepr, SchedKind, Value};
pub use vm::{BuildInputs, Halt, PanicInfo, Vm};

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

/// Compiles + runs every `test { ... }` block in `prog`, returning a
/// [`TestReport`]. Each test runs on a fresh fiber with clean scheduler state, so
/// one test cannot leak fibers into the next. A `catch_unwind` backstop maps any
/// stray internal Rust panic to a failing report rather than aborting.
pub fn run_tests(prog: &MirProgram) -> TestReport {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut vm = Vm::new(prog);
        let (passed, failed, lines) = vm.run_tests();
        (passed, failed, lines, vm.stdout, vm.stderr)
    }));
    match result {
        Ok((passed, failed, lines, stdout, stderr)) => TestReport {
            passed,
            failed,
            lines,
            stdout,
            stderr,
        },
        Err(_) => TestReport {
            passed: 0,
            failed: 1,
            lines: vec!["internal VM error (Rust panic) running tests".to_string()],
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    }
}

/// The arguments to a run: the build mode (informs ReleaseFast behaviour) and the
/// forwarded program argv (reserved; `main(sys)` takes no argv in v0.8).
pub struct RunArgs {
    /// The build mode the program was lowered under.
    pub mode: BuildMode,
    /// The program arguments (forwarded; unused by the current `main` shape).
    pub argv: Vec<String>,
}

impl RunArgs {
    /// The default run arguments for a given build mode.
    pub fn new(mode: BuildMode) -> RunArgs {
        RunArgs {
            mode,
            argv: Vec::new(),
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(prog, args)));
    match result {
        Ok((outcome, code, out, err)) => {
            // Flush the program's captured streams before any diagnostic line.
            let _ = std::io::stdout().write_all(&out);
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().write_all(&err);
            match outcome {
                RunOutcome::Ok => {}
                RunOutcome::Errored(name) => {
                    let _ = writeln!(std::io::stderr(), "error: {name}");
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
    let _ = args;
    run_inner(prog, RunArgs::new(prog.mode))
}

/// The shared core: builds a VM, runs `main`, and maps the [`Halt`] to an outcome
/// + exit code. Returns the captured stdout/stderr buffers.
fn run_inner(prog: &MirProgram, _args: RunArgs) -> (RunOutcome, i32, Vec<u8>, Vec<u8>) {
    let (outcome, code, out, err, _count) = run_inner_metered(prog);
    (outcome, code, out, err)
}

/// Like [`run_captured`], but also returns the number of VM instructions
/// executed — the deterministic, reproducible metric the benchmark harness uses
/// to compare Debug vs ReleaseFast. The VM is single-threaded and its dispatch is
/// data-deterministic, so this count is identical across runs of the same MIR.
///
/// The existing `run_captured`/`run_program` signatures are intentionally left
/// unchanged so no caller breaks; this is an additive entry point.
pub fn run_metered(prog: &MirProgram) -> (RunOutcome, i32, Vec<u8>, Vec<u8>, u64) {
    run_inner_metered(prog)
}

/// The metered shared core: identical to [`run_inner`] but also reports the
/// executed-instruction count.
fn run_inner_metered(prog: &MirProgram) -> (RunOutcome, i32, Vec<u8>, Vec<u8>, u64) {
    let mut vm = Vm::new(prog);
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
    (outcome, code, vm.stdout, vm.stderr, count)
}

/// Maps an `i32` exit code to a process [`ExitCode`], clamping to a `u8`.
fn exit_code(code: i32) -> ExitCode {
    ExitCode::from((code & 0xff) as u8)
}
