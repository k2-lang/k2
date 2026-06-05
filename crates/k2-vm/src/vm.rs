//! The register virtual machine: frames, the dispatch loop, intrinsics, and the
//! clean-panic discipline.
//!
//! The VM executes the compiled bytecode of a [`MirProgram`]. It is *iterative
//! across calls*: a k2 call pushes a heap-allocated [`Frame`] onto a `Vec`, never
//! recursing in Rust, so deep k2 recursion (fib) costs heap frames rather than
//! Rust stack. A failed safety check, a `Trap`, or an `Unreachable` terminator
//! becomes a [`Halt::Panic`] that the public entry turns into a `panic:` line on
//! stderr and a nonzero exit — never an uncontrolled Rust panic or abort. An
//! instruction budget guards against a runaway loop so the VM always terminates.

use std::rc::Rc;

use k2_mir::{BinOp, CastKind, DiscrKind, FnId, MirProgram, SliceMeta, TrapReason, UnOp};
use k2_types::TypeId;

use crate::build_graph::{
    Artifact, ArtifactKind, BuildGraph, BuildOptVal, DeclaredOption, ModuleNode, OptMode, StepNode,
    TargetTriple,
};
use crate::compile::{compile_program, default_value, int_repr_of};
use crate::fmt::format_into;
use crate::heap::{Heap, HeapFault, Ptr};
use crate::isa::{AggregateKind, CompiledFn, Instr, IntrinsicId, Reg, StoreStep};
use crate::sched::{BlockReason, FiberId, FiberState, Frame, Scheduler};
use crate::value::{Capability, IntRepr, SchedKind, Value};

/// The build-time inputs the driver seeds into the VM before running `build(b)`:
/// the resolved target/optimize choices and the `-D` option map. The `*Build`
/// intrinsics read these (e.g. `b.standardTarget()` returns `target`,
/// `b.option(...)` looks up the `-D` map) and record the rest into a
/// [`BuildGraph`].
#[derive(Clone, Debug)]
pub struct BuildInputs {
    /// The resolved target triple (`-Dtarget`, default host).
    pub target: TargetTriple,
    /// The optimization mode (`-Doptimize`, default Debug).
    pub optimize: OptMode,
    /// The `-Dkey=value` option map (deterministic order via the driver's
    /// `BTreeMap`).
    pub dopts: Vec<(String, String)>,
}

/// The mutable build-recording state the `*Build` floor writes into. Present only
/// while `build(b)` runs; ordinary `run`/`test` leave it `None`.
struct BuildContext {
    /// The driver-seeded inputs (target/optimize/`-D` map).
    inputs: BuildInputs,
    /// The graph being recorded, in creation order.
    graph: BuildGraph,
}

/// How far the VM may step before it gives up on a presumed-nonterminating
/// program. Real corpus programs finish in a few thousand steps; this is still
/// five orders of magnitude larger so no genuine program is cut off, yet small
/// enough to bound the worst case.
///
/// THIS is the deterministic termination guarantee: the step counter advances by
/// exactly one per executed instruction, identically on every machine and every
/// run, so a program either finishes within the budget on *all* machines or trips
/// the budget on *all* machines — its pass/fail outcome never varies with host
/// speed. Every real program (and every deliberately-nonterminating one, e.g. an
/// infinite `yieldNow` loop) is bounded by this alone; the [`WALL_DEADLINE`]
/// below is only a non-deterministic backstop layered on top, never the metric a
/// correct program's outcome depends on.
const STEP_BUDGET: u64 = 200_000_000;

/// The wall-clock deadline: a NON-DETERMINISTIC safety backstop, NOT the
/// termination guarantee. The deterministic [`STEP_BUDGET`] above is the real
/// bound and is sufficient to terminate every program on its own; this clause
/// only exists so a runaway loop is reported within a few seconds of real time
/// even under the (slower) debug build, where exhausting 2e8 steps would
/// otherwise take ~30s.
///
/// Because it reads `Instant::elapsed()`, a program that runs *near* this
/// boundary could trip it on a slow/loaded machine and not on a fast one — i.e.
/// this guard alone is machine-dependent. That is acceptable ONLY because no
/// real or corpus program runs anywhere near 5 s of wall time *or* near the 2e8
/// step budget: every genuine program finishes in milliseconds / a few thousand
/// steps, far below both guards, and any true infinite loop trips the
/// deterministic step budget first (a yield/spin loop burns one step per turn).
/// So in practice the wall-clock clause is never the deciding guard; it is a pure
/// backstop. Do NOT make any program's correctness depend on it.
const WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// How often (in steps) the wall-clock backstop is polled. Polling is itself
/// deterministic (every 2^20 steps), so only the `elapsed() > WALL_DEADLINE`
/// comparison — never *when* it is checked — carries the non-determinism noted on
/// [`WALL_DEADLINE`].
const WALL_CHECK_INTERVAL: u64 = 1 << 20;

/// The maximum call depth before a clean `stack overflow` panic (rather than a
/// Rust stack overflow — though the VM does not recurse in Rust, this bounds
/// pathological heap-frame growth).
const MAX_DEPTH: usize = 200_000;

/// A reason the VM stopped: a clean program panic, an error that escaped `main`,
/// or an explicit exit.
#[derive(Clone, Debug)]
pub enum Halt {
    /// A safety check failed or a trap/unreachable fired.
    Panic(PanicInfo),
    /// `main` returned an error value (its name is printed by the caller).
    ProgramError(u16),
    /// An explicit process exit with a code.
    Exit(i32),
}

/// The detail of a clean panic.
#[derive(Clone, Debug)]
pub struct PanicInfo {
    /// Why the panic fired.
    pub reason: TrapReason,
    /// An optional extra detail (e.g. the offending index/length).
    pub detail: Option<String>,
}

impl PanicInfo {
    /// A panic describing an internal toolchain failure (e.g. a caught Rust
    /// panic), used by the public entry points' `catch_unwind` backstops.
    pub fn internal(what: &str) -> PanicInfo {
        PanicInfo {
            reason: TrapReason::Panic,
            detail: Some(format!("internal VM error: {what}")),
        }
    }

    /// The human-readable panic message (without the `panic: ` prefix).
    ///
    /// Kept **byte-identical** to the native backend's `trap_message`
    /// (`crates/k2-codegen/src/lower.rs`) so a trap prints the same text on both
    /// backends. When changing a string here, update that table too.
    pub fn message(&self) -> String {
        let base = match self.reason {
            TrapReason::Bounds => "index out of bounds",
            TrapReason::Overflow => "integer overflow",
            TrapReason::DivByZero => "division by zero",
            TrapReason::NegOverflow => "negation of minimum integer",
            TrapReason::NarrowLoss => "cast truncated value",
            TrapReason::LenMismatch => "for loop length mismatch",
            TrapReason::Unreachable => "reached unreachable code",
            TrapReason::Panic => "reached @panic / unwrapped null",
        };
        match &self.detail {
            Some(d) => format!("{base}: {d}"),
            None => base.to_string(),
        }
    }
}

/// The kind (strategy) of an allocator instance in the registry. The kind
/// decides how `free`/`realloc`/`deinit` behave; allocation itself always goes
/// through the one shared managed [`Heap`], so pointers/slices from every kind
/// interoperate freely.
enum AllocatorKind {
    /// The program-wide default (`sys.heap`): plain allocate/free, no tracking.
    Default,
    /// A leak-/double-free-/use-after-free-checking general-purpose allocator
    /// (also the testing allocator). Tracks every live cell; `deinit` reports a
    /// leak, a double/foreign free traps.
    Gpa,
    /// An arena: `free` is a no-op; `deinit` frees every cell at once. Records the
    /// cells it handed out.
    Arena,
    /// A fixed-buffer allocator carving from a caller-provided `[]u8`: it bumps an
    /// offset into one backing cell; `free` is a no-op (reset only).
    FixedBuffer { buf: Ptr, cap: usize },
}

/// One allocator instance: its kind plus the per-kind bookkeeping the VM keeps
/// over the shared heap (the set of live cells for a GPA, the handed-out cells
/// for an arena, the bump offset for a fixed-buffer allocator).
struct AllocatorState {
    kind: AllocatorKind,
    /// Cells currently live (for a GPA: the leak set; for an arena: the cells to
    /// free at `deinit`).
    live: Vec<u32>,
    /// Cells already freed through this allocator (for a GPA: the double-free
    /// set).
    freed: Vec<u32>,
    /// The fixed-buffer bump offset (elements consumed from `buf`).
    offset: usize,
}

impl AllocatorState {
    /// A fresh allocator instance of `kind` with empty bookkeeping.
    fn new(kind: AllocatorKind) -> AllocatorState {
        AllocatorState {
            kind,
            live: Vec::new(),
            freed: Vec::new(),
            offset: 0,
        }
    }
}

/// The virtual machine.
pub struct Vm<'p> {
    prog: &'p MirProgram,
    compiled: Vec<CompiledFn>,
    heap: Heap,
    /// The deterministic cooperative fiber scheduler. The program runs as a root
    /// fiber; `spawn`/channel/mutex/await suspend a fiber to a yield point and the
    /// event loop interleaves ready fibers. A non-concurrent program runs as a
    /// single root fiber whose dispatch is byte-for-byte the old single-stack path.
    sched: Scheduler,
    /// The per-run allocator registry, indexed by handle id. Slot `0` is the
    /// program-wide default (`sys.heap`). The `std.heap.*` allocators mint
    /// further slots (GPA, arena, fixed-buffer) via `@allocId`, and every
    /// `@*Raw`/`@gpaDeinit`/`@arenaDeinit` op dispatches on a handle id into this
    /// table, so different allocator *kinds* behave differently over the one
    /// shared managed [`Heap`].
    allocators: Vec<AllocatorState>,
    /// The VM's monotonic clock, in nanoseconds. Starts at zero and advances on
    /// `@clockSleep`, so `@clockNow(0)` is deterministic across runs (the spec's
    /// `FakeClock` is pure k2 over this same monotonic shape).
    clock_nanos: u64,
    /// The PRNG state (splitmix64). Seeded deterministically so `sys.random` is
    /// reproducible unless a `--seed` is supplied.
    rng: u64,
    /// Whether this run strips safety checks (ReleaseFast): the GPA/testing leak
    /// and double-free trackers are no-ops, mirroring the heap's `ignore_liveness`.
    checks_off: bool,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
    /// Steps remaining before the budget is exhausted.
    budget: u64,
    /// The number of instructions executed so far. This is the deterministic,
    /// reproducible "executed VM instructions" metric the benchmark harness
    /// reports: it is incremented exactly once per dispatched instruction,
    /// independent of wall-clock, so it is identical across runs of the same
    /// program. (`budget` counts *down* and also factors in the wall-clock guard,
    /// so it is not a usable forward counter on its own.)
    instr_count: u64,
    /// When the run started, for the wall-clock termination deadline.
    started: std::time::Instant,
    /// The build-recording context, present only while `build(b)` runs (the
    /// `*Build` capability floor writes into it). `None` for ordinary runs.
    build: Option<BuildContext>,
    /// The error-return trace captured from the root fiber when an error escaped
    /// `main` (newest-first). Empty in ReleaseFast and for non-error exits. Read
    /// via [`Vm::escaped_trace`] after `run_main`.
    escaped_trace: Vec<crate::trace::TraceFrame>,
}

impl<'p> Vm<'p> {
    /// Builds a VM over a lowered program, compiling every function up front.
    pub fn new(prog: &'p MirProgram) -> Vm<'p> {
        let release_fast = matches!(prog.mode, k2_mir::BuildMode::ReleaseFast);
        Vm {
            prog,
            compiled: compile_program(prog),
            heap: Heap::new(release_fast),
            sched: Scheduler::new(),
            // Slot 0 is the always-present default allocator (`sys.heap`): plain
            // heap allocation with no extra tracking.
            allocators: vec![AllocatorState::new(AllocatorKind::Default)],
            clock_nanos: 0,
            // A fixed nonzero seed: deterministic but not all-zero (splitmix64 of
            // 0 still produces a good stream, but a recognizable seed aids debug).
            rng: 0x9E37_79B9_7F4A_7C15,
            checks_off: release_fast,
            stdout: Vec::new(),
            stderr: Vec::new(),
            budget: STEP_BUDGET,
            instr_count: 0,
            started: std::time::Instant::now(),
            build: None,
            escaped_trace: Vec::new(),
        }
    }

    /// The error-return trace captured when an error escaped `main` (newest
    /// first). Empty unless `run_main` returned [`Halt::ProgramError`] in a
    /// Debug/ReleaseSafe build with `try`-propagated errors.
    pub fn escaped_trace(&self) -> &[crate::trace::TraceFrame] {
        &self.escaped_trace
    }

    /// The number of instructions executed so far — the deterministic metric the
    /// benchmark harness reports (Debug vs ReleaseFast).
    pub fn instr_count(&self) -> u64 {
        self.instr_count
    }

    /// Runs `main(sys)` to completion. On success returns `Ok(())`; otherwise a
    /// [`Halt`] describing the panic / program error / exit.
    ///
    /// The main program is the **root fiber**. The event loop then interleaves it
    /// with any fibers it spawns, running each to its next yield point, until all
    /// complete (or a deadlock is detected). For a program that never spawns this
    /// is a single fiber whose dispatch matches the old single-stack path exactly.
    pub fn run_main(&mut self) -> Result<(), Halt> {
        let main = self.find_main().ok_or(Halt::Exit(0))?;
        // Whether `main` declares a concrete integer return type. Such a `main`
        // propagates its result as the process exit code (truncated to a u8),
        // matching the native backend's `_start` shim. An `!void` main keeps the
        // success->0 / escaped-error->1 convention on both backends.
        let int_main = matches!(
            self.prog.arena.get(self.prog.funcs[main.index()].ret),
            k2_types::Type::Int { .. }
        );
        // Build the root `*System` capability and seed the root fiber.
        let sys = Value::Cap(Capability::System);
        let root = self.spawn_fiber_for(main, vec![sys])?;
        self.event_loop()?;
        // Inspect the root fiber's result.
        match self.sched.fibers[root as usize].result.take() {
            Some(Value::ErrVal(tag)) => {
                // Capture the propagation trace accumulated on the root fiber so
                // the driver can print it after the `error: <name>` header.
                self.escaped_trace =
                    std::mem::take(&mut self.sched.fibers[root as usize].err_trace);
                Err(Halt::ProgramError(tag))
            }
            // `pub fn main(...) <Int>`: exit with the integer result (low 8 bits),
            // agreeing with native's integer-main exit-code convention.
            Some(v) if int_main => {
                let code = v.as_i128().unwrap_or(0);
                Err(Halt::Exit((code as u8) as i32))
            }
            _ => Ok(()),
        }
    }

    /// Finds the entry `main` function id.
    fn find_main(&self) -> Option<FnId> {
        if let Some(f) = self.prog.funcs.iter().find(|f| f.name == "main") {
            return Some(f.id);
        }
        self.prog.entries.first().copied()
    }

    /// Finds the `pub fn build(b: *Build)` entry function id, if present.
    fn find_build(&self) -> Option<FnId> {
        self.prog
            .funcs
            .iter()
            .find(|f| f.name == "build")
            .map(|f| f.id)
    }

    /// Runs `build(b)` on the VM with a `*Build` capability and returns the
    /// recorded [`BuildGraph`]. This is the faithful realization of the roadmap's
    /// "executed by the comptime engine": `build(b)` is ordinary k2 run on the
    /// same VM as any program, but with a `*Build` value backed by recording
    /// intrinsics that build a graph (no I/O, no real allocation).
    pub fn run_build(&mut self, inputs: BuildInputs) -> Result<BuildGraph, Halt> {
        let build = self.find_build().ok_or_else(|| {
            Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some("build.k2 has no `pub fn build(b: *Build)` entry".to_string()),
            })
        })?;
        let graph = BuildGraph::new(inputs.target.clone(), inputs.optimize);
        self.build = Some(BuildContext { inputs, graph });
        // The `*Build` capability is a fresh opaque handle, like `*System`.
        let cap = Value::Cap(Capability::Build);
        let root = self.spawn_fiber_for(build, vec![cap])?;
        self.event_loop()?;
        let _ = root;
        Ok(self
            .build
            .take()
            .map(|b| b.graph)
            .unwrap_or_else(|| BuildGraph::new(TargetTriple::host(), OptMode::Debug)))
    }

    /// Runs every `test { ... }` block in `prog` on a fresh fiber, returning
    /// `(passed, failed, report_lines)`. Each test function is named `"test …"`
    /// by the MIR lowering; a test that returns an error (its `!void` carries an
    /// `ErrVal`) or panics is a failure. This reuses the existing test lowering;
    /// the VM only needs this entry point.
    pub fn run_tests(&mut self) -> (usize, usize, Vec<String>) {
        // Collect the test entries up front (name + id) so the borrow of `prog`
        // does not overlap the fiber runs.
        let tests: Vec<(FnId, String)> = self
            .prog
            .funcs
            .iter()
            .filter(|f| f.name == "test" || f.name.starts_with("test "))
            .map(|f| (f.id, f.name.clone()))
            .collect();
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut report = Vec::new();
        for (fid, name) in tests {
            match self.run_one_test(fid) {
                Ok(()) => {
                    passed += 1;
                    report.push(format!("{name} ... ok"));
                }
                Err(detail) => {
                    failed += 1;
                    report.push(format!("{name} ... FAILED ({detail})"));
                }
            }
        }
        (passed, failed, report)
    }

    /// Runs a single test function on a fresh fiber + clean scheduler state,
    /// returning `Ok(())` on pass or `Err(detail)` on an error/panic.
    fn run_one_test(&mut self, fid: FnId) -> Result<(), String> {
        // Reset the scheduler so each test runs on its own clean fiber set (a
        // prior test's stale fibers must not leak into this one).
        self.sched = Scheduler::new();
        let root = match self.spawn_fiber_for(fid, Vec::new()) {
            Ok(r) => r,
            Err(h) => return Err(halt_detail(&h)),
        };
        match self.event_loop() {
            Ok(()) => {}
            Err(h) => return Err(halt_detail(&h)),
        }
        match self.sched.fibers[root as usize].result.take() {
            Some(Value::ErrVal(tag)) => {
                let name = self
                    .prog
                    .err_names
                    .get(&k2_mir::ErrTag(tag))
                    .cloned()
                    .unwrap_or_else(|| format!("error{tag}"));
                Err(format!("error.{name}"))
            }
            _ => Ok(()),
        }
    }

    /// Builds the root [`Frame`] for `fnid` seeded with `args`, registers a fresh
    /// fiber for it (enqueued Ready), and returns its id.
    fn spawn_fiber_for(&mut self, fnid: FnId, args: Vec<Value>) -> Result<FiberId, Halt> {
        let frame = self.make_frame(fnid, args, 0)?;
        let id = self.sched.spawn_fiber(frame);
        // Box any address-taken params of the new fiber's root frame.
        let prev = self.sched.current;
        self.sched.current = id;
        self.init_addr_taken_params(fnid);
        self.sched.current = prev;
        Ok(id)
    }

    /// Pushes a new frame for `fnid` onto the *current* fiber's stack, seeding its
    /// parameter registers with `args` and boxing any address-taken params.
    fn push_frame(&mut self, fnid: FnId, args: Vec<Value>, ret_reg: Reg) -> Result<(), Halt> {
        if self.sched.cur_frames().len() >= MAX_DEPTH {
            return Err(Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some("stack overflow".to_string()),
            }));
        }
        let frame = self.make_frame(fnid, args, ret_reg)?;
        self.sched.cur_frames_mut().push(frame);
        // Initialize address-taken parameters into heap homes immediately, so a
        // `&param` / `self.*` receiver works. (Locals get their homes at their
        // StorageLive via InitAddrLocal.)
        self.init_addr_taken_params(fnid);
        Ok(())
    }

    /// Builds (but does not push) a fresh [`Frame`] for `fnid` with `args` in the
    /// parameter registers.
    fn make_frame(&self, fnid: FnId, args: Vec<Value>, ret_reg: Reg) -> Result<Frame, Halt> {
        let cf = &self.compiled[fnid.index()];
        let mut regs = vec![Value::Unit; cf.num_regs];
        // Params occupy registers 1..=args.len() (register 0 is the return slot).
        for (i, a) in args.into_iter().enumerate() {
            if i + 1 < regs.len() {
                regs[i + 1] = a;
            }
        }
        Ok(Frame {
            fnid,
            regs,
            pc: 0,
            ret_reg,
        })
    }

    /// Boxes any `address_taken` parameter of the current fiber's top frame into a
    /// heap cell at frame entry.
    fn init_addr_taken_params(&mut self, fnid: FnId) {
        let cf = &self.compiled[fnid.index()];
        let func = &self.prog.funcs[fnid.index()];
        let nparams = func.params.len();
        // Collect the param register indices that are address-taken.
        let to_box: Vec<usize> = (1..=nparams).filter(|&i| cf.addr_taken[i]).collect();
        for i in to_box {
            let cur = self.sched.cur_frames().last().unwrap().regs[i].clone();
            let ptr = self.heap.alloc_one(cur);
            self.sched.cur_frames_mut().last_mut().unwrap().regs[i] = Value::Ptr(ptr);
        }
    }

    /// The deterministic event loop: pick the next Ready fiber (FIFO), run it to
    /// its next suspend/yield/completion, and repeat until every fiber is Done. An
    /// empty ready queue with live fibers is a clean deadlock (never a hang).
    fn event_loop(&mut self) -> Result<(), Halt> {
        loop {
            let Some(fid) = self.sched.ready.pop_front() else {
                if self.sched.all_done() {
                    return Ok(());
                }
                return Err(self.deadlock_panic());
            };
            if matches!(self.sched.fibers[fid as usize].state, FiberState::Done) {
                continue; // a stale ready-queue entry (already completed); skip.
            }
            self.sched.current = fid;
            self.sched.fibers[fid as usize].state = FiberState::Running;
            match self.run_fiber()? {
                Suspend::Blocked => { /* parked by the intrinsic; not re-enqueued */ }
                Suspend::Yielded => {
                    self.sched.fibers[fid as usize].state = FiberState::Ready;
                    self.sched.ready.push_back(fid);
                }
                Suspend::Completed(v) => self.complete_fiber(fid, v),
            }
        }
    }

    /// Runs the *current* fiber's dispatch loop until it suspends (blocks/yields)
    /// or its root frame returns. The instruction budget and the `instr_count`
    /// metric stay global across all fibers, so a runaway program still terminates
    /// with the existing clean budget panic.
    ///
    /// Termination is guarded by two clauses below. The first — `budget == 0` — is
    /// the DETERMINISTIC guarantee: one decrement per executed instruction, so the
    /// outcome is identical on every machine and every run (see [`STEP_BUDGET`]).
    /// The second — the [`WALL_DEADLINE`] elapsed check — is only a
    /// non-deterministic backstop (see its doc); it never decides the outcome of a
    /// real program, every one of which finishes far below both bounds.
    fn run_fiber(&mut self) -> Result<Suspend, Halt> {
        loop {
            // Deterministic step budget (the real guarantee) OR the wall-clock
            // backstop (a machine-dependent safety net, polled cheaply). See the
            // constants' docs: only the first clause is relied upon for correctness.
            if self.budget == 0
                || (self.budget.is_multiple_of(WALL_CHECK_INTERVAL)
                    && self.started.elapsed() > WALL_DEADLINE)
            {
                return Err(Halt::Panic(PanicInfo {
                    reason: TrapReason::Panic,
                    detail: Some(
                        "instruction budget exhausted (possible infinite loop)".to_string(),
                    ),
                }));
            }
            self.budget -= 1;
            self.instr_count += 1;

            let frame = self.sched.cur_frames_mut().last_mut().unwrap();
            let fnid = frame.fnid;
            let pc = frame.pc;
            let instr = self.compiled[fnid.index()].code[pc].clone();
            frame.pc += 1;

            match self.step(instr)? {
                Flow::Next | Flow::Jumped => {}
                // A blocking intrinsic already parked the current fiber (recording
                // its dst as the resume register); just stop running it.
                Flow::Suspend => return Ok(Suspend::Blocked),
                // An explicit `@schedYield`: the fiber is still Ready, re-enqueued
                // by the event loop next tick (fair round-robin).
                Flow::Yield => return Ok(Suspend::Yielded),
                Flow::Returned(v) => {
                    // Pop the returning frame; deliver its value to the caller.
                    let done = self.sched.cur_frames_mut().pop().unwrap();
                    if self.sched.cur_frames().is_empty() {
                        // The fiber's root frame returned: the fiber completes.
                        return Ok(Suspend::Completed(v));
                    }
                    // Write the result into the caller's destination register.
                    let caller = self.sched.cur_frames_mut().last_mut().unwrap();
                    Self::set_reg(caller, done.ret_reg, v);
                }
            }
        }
    }

    /// Marks fiber `fid` completed with result `v` and wakes every joiner (each
    /// receives the result into its parked `resume_reg`).
    fn complete_fiber(&mut self, fid: FiberId, v: Value) {
        let f = &mut self.sched.fibers[fid as usize];
        f.result = Some(v.clone());
        f.state = FiberState::Done;
        let joiners = std::mem::take(&mut f.joiners);
        // Deliver the unwrapped result into each joiner's parked await register.
        let delivered = unwrap_task_result(v);
        for j in joiners {
            self.sched.wake(j, Some(delivered.clone()));
        }
    }

    /// The clean "all fibers blocked" deadlock diagnostic. Because every block
    /// reason has an explicit waker and the ready queue is the only progress
    /// source, an empty ready queue with live fibers is provably a deadlock; this
    /// is reported immediately (no timeout) as a clean panic, never a hang.
    fn deadlock_panic(&self) -> Halt {
        let blocked = self.sched.blocked_count();
        // Summarize what each stuck fiber is waiting on, so the diagnostic names
        // the culprit (e.g. "channel recv") rather than just a count.
        let mut waiting: Vec<String> = self
            .sched
            .fibers
            .iter()
            .filter_map(|f| match &f.state {
                FiberState::Blocked(reason) => Some(reason.label()),
                _ => None,
            })
            .collect();
        waiting.sort_unstable();
        waiting.dedup();
        let on = waiting.join(", ");
        Halt::Panic(PanicInfo {
            reason: TrapReason::Panic,
            detail: Some(format!(
                "deadlock: all {blocked} fiber(s) are blocked with no runnable task \
                 (waiting on: {on}) — no waker can ever fire"
            )),
        })
    }

    /// Executes one instruction in the current top frame.
    fn step(&mut self, instr: Instr) -> Result<Flow, Halt> {
        match instr {
            Instr::ConstK { dst, k } => {
                let v = self.compiled[self.cur_fnid().index()].consts[k as usize].clone();
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Move { dst, src } => {
                let v = self.get(src);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Bin {
                dst,
                op,
                a,
                b,
                repr,
            } => {
                let av = self.get(a);
                let bv = self.get(b);
                let v = self.eval_binary(op, &av, &bv, repr)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Un { dst, op, a, repr } => {
                let av = self.get(a);
                let v = self.eval_unary(op, &av, repr)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Cast {
                dst,
                kind,
                a,
                to,
                to_float,
            } => {
                let av = self.get(a);
                let v = self.eval_cast(kind, &av, to, to_float);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::PackStruct { dst, a, fields, to } => {
                let av = self.get(a);
                let v = self.pack_struct(&av, &fields, to)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::UnpackStruct { dst, a, fields } => {
                let av = self.get(a);
                let v = self.unpack_struct(&av, &fields);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Jump { target } => {
                self.sched.cur_frames_mut().last_mut().unwrap().pc = target;
                Ok(Flow::Jumped)
            }
            Instr::Branch {
                cond,
                then_pc,
                else_pc,
            } => {
                let c = self.get(cond).as_bool().unwrap_or(false);
                self.sched.cur_frames_mut().last_mut().unwrap().pc =
                    if c { then_pc } else { else_pc };
                Ok(Flow::Jumped)
            }
            Instr::Switch {
                scrut,
                arms,
                default,
            } => {
                let s = self.get(scrut).as_i128().unwrap_or(i128::MIN);
                let target = arms
                    .iter()
                    .find(|(v, _)| *v == s)
                    .map(|(_, t)| *t)
                    .unwrap_or(default);
                self.sched.cur_frames_mut().last_mut().unwrap().pc = target;
                Ok(Flow::Jumped)
            }
            Instr::Return { src } => {
                let v = self.get(src);
                Ok(Flow::Returned(v))
            }
            Instr::ReturnErr { src, site } => {
                // Record an error-return-trace frame for this `try` site on the
                // current fiber, then return exactly like `Instr::Return`. The
                // frame is appended (newest first), building the propagation
                // chain as the error unwinds the stack.
                let fnid = self.cur_fnid();
                let s = &self.compiled[fnid.index()].trace_sites[site as usize];
                let frame = crate::trace::TraceFrame {
                    fn_name: s.fn_name.clone(),
                    line: s.line,
                    col: s.col,
                };
                let cur = self.sched.current;
                self.sched.fibers[cur as usize].err_trace.push(frame);
                let v = self.get(src);
                Ok(Flow::Returned(v))
            }
            Instr::Trap { reason } => Err(Halt::Panic(PanicInfo {
                reason,
                detail: None,
            })),
            Instr::Unreachable => Err(Halt::Panic(PanicInfo {
                reason: TrapReason::Unreachable,
                detail: None,
            })),
            Instr::Call { dst, func, args } => {
                let argv: Vec<Value> = args.iter().map(|&r| self.get(r)).collect();
                self.push_frame(func, argv, dst)?;
                Ok(Flow::Jumped)
            }
            Instr::Intrinsic {
                dst,
                id,
                recv,
                args,
            } => {
                let recv_v = recv.map(|r| self.get(r));
                let argv: Vec<Value> = args.iter().map(|&r| self.get(r)).collect();
                match self.dispatch_intrinsic(&id, recv_v, &argv)? {
                    IntrinsicOutcome::Value(v) => {
                        self.set_cur(dst, v);
                        Ok(Flow::Next)
                    }
                    // A blocking intrinsic: park the current fiber, recording `dst`
                    // as the register a later wake delivers the value into. The
                    // `pc` was already advanced, so on resume the fiber continues
                    // right after this instruction.
                    IntrinsicOutcome::Suspend(reason) => {
                        self.sched.block_current(reason, dst);
                        Ok(Flow::Suspend)
                    }
                    // An explicit cooperative yield: the fiber stays Ready and, on
                    // resume, continues to the statement after this one.
                    IntrinsicOutcome::Yield => {
                        self.set_cur(dst, Value::Unit);
                        Ok(Flow::Yield)
                    }
                    // A drive-to-quiescence yield (`@schedRun`): rewind the pc to
                    // re-point at *this* instruction so it re-executes — and so
                    // re-tests `other_runnable()` — when the fiber is next scheduled.
                    // `run_fiber` advanced `pc` by 1 before `step`, so we undo that
                    // single advance here. Nothing is written to `dst` yet (the value
                    // is produced only when the drain finally returns a `Value`).
                    IntrinsicOutcome::YieldReexec => {
                        let frame = self.sched.cur_frames_mut().last_mut().unwrap();
                        frame.pc -= 1;
                        Ok(Flow::Yield)
                    }
                }
            }
            Instr::LoadField { dst, base, idx } => {
                let bv = self.get(base);
                let v = self.field_of(&bv, idx)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::LoadIndex { dst, base, index } => {
                let bv = self.get(base);
                let i = self.get(index);
                let v = self.index_of(&bv, &i)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::LoadDeref { dst, ptr } => {
                let pv = self.get(ptr);
                let v = self.deref(&pv)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::LoadSliceMeta { dst, base, which } => {
                let bv = self.get(base);
                let v = self.slice_meta(&bv, which)?;
                // A `.ptr` taken from a by-value array boxes the array into a heap
                // cell; reflect that back into the base register so repeated meta
                // reads (e.g. a sub-slice's `.ptr` and `.len`) share one backing
                // allocation and the local aliases the slice's data.
                if let (SliceMeta::Ptr, Value::Ptr(p)) = (which, &v) {
                    if matches!(bv, Value::Array(_)) {
                        self.set_cur(base, Value::Ptr(*p));
                    }
                }
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::LoadPayload { dst, base } => {
                let bv = self.get(base);
                let v = self.payload_of(&bv);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Discr { dst, base, kind } => {
                let bv = self.get(base);
                let v = self.discriminant(&bv, kind);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::Store { base, steps, src } => {
                let v = self.get(src);
                self.store_place(base, &steps, v)?;
                Ok(Flow::Next)
            }
            Instr::Ref {
                dst,
                base,
                base_is_local,
                steps,
            } => {
                let v = self.take_ref(base, base_is_local, &steps)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::MakeSlice {
                dst,
                ptr,
                offset,
                len,
            } => {
                let pv = self.get(ptr);
                let ov = self.get(offset);
                let lv = self.get(len);
                let v = self.make_slice(&pv, &ov, &lv);
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::MakeSome { dst, src } => {
                let v = self.get(src);
                self.set_cur(dst, Value::Optional(Some(Box::new(v))));
                Ok(Flow::Next)
            }
            Instr::MakeNull { dst } => {
                self.set_cur(dst, Value::Optional(None));
                Ok(Flow::Next)
            }
            Instr::MakeOk { dst, src } => {
                let v = self.get(src);
                self.set_cur(dst, Value::ErrOk(Box::new(v)));
                Ok(Flow::Next)
            }
            Instr::MakeErr { dst, tag } => {
                // A fresh error origin reseeds this fiber's error-return trace, so
                // a brand-new error never inherits a previously-caught error's
                // propagation chain. The subsequent `try` sites append onto it.
                let cur = self.sched.current;
                self.sched.fibers[cur as usize].err_trace.clear();
                self.set_cur(dst, Value::ErrVal(tag));
                Ok(Flow::Next)
            }
            Instr::Aggregate { dst, kind, fields } => {
                let vs: Vec<Value> = fields.iter().map(|&r| self.get(r)).collect();
                let v = match kind {
                    AggregateKind::Array => Value::Array(Rc::new(vs)),
                    AggregateKind::Struct | AggregateKind::Tuple => Value::Struct(Rc::new(vs)),
                };
                self.set_cur(dst, v);
                Ok(Flow::Next)
            }
            Instr::InitAddrLocal { reg, init_k } => {
                let init = self.compiled[self.cur_fnid().index()].consts[init_k as usize].clone();
                let ptr = self.heap.alloc_one(init);
                self.set_cur(reg, Value::Ptr(ptr));
                Ok(Flow::Next)
            }
        }
    }

    // ---- register access ----------------------------------------------

    /// The current frame's function id (of the running fiber).
    fn cur_fnid(&self) -> FnId {
        self.sched.cur_frames().last().unwrap().fnid
    }

    /// Reads register `r` of the current (running fiber's top) frame.
    fn get(&self, r: Reg) -> Value {
        self.sched.cur_frames().last().unwrap().regs[r as usize].clone()
    }

    /// Writes register `r` of the current (running fiber's top) frame.
    fn set_cur(&mut self, r: Reg, v: Value) {
        let frame = self.sched.cur_frames_mut().last_mut().unwrap();
        Self::set_reg(frame, r, v);
    }

    /// Writes register `r` of an arbitrary frame.
    fn set_reg(frame: &mut Frame, r: Reg, v: Value) {
        frame.regs[r as usize] = v;
    }

    // ---- arithmetic ----------------------------------------------------

    /// Evaluates a binary operation, respecting width/signedness for integer
    /// results. Division/remainder avoid a Rust panic on a zero divisor (the
    /// safety check already traps in checked builds; in ReleaseFast we return a
    /// defined sentinel).
    fn eval_binary(&self, op: BinOp, a: &Value, b: &Value, repr: IntRepr) -> Result<Value, Halt> {
        // Float arithmetic when either operand is a float.
        if matches!(a, Value::Float(_)) || matches!(b, Value::Float(_)) {
            let x = a.as_f64().unwrap_or(0.0);
            let y = b.as_f64().unwrap_or(0.0);
            return Ok(self.float_binary(op, x, y));
        }
        let x = a.as_i128().unwrap_or(0);
        let y = b.as_i128().unwrap_or(0);
        // Relational comparisons must use the OPERANDS' width/signedness, not the
        // instruction's `repr` — for a comparison `repr` is the *result* type
        // (`bool`, width 0), which would force the signed branch and mis-order a
        // `u128` whose high bit is set (it is stored as a negative `i128`). The
        // operand values carry their own repr, so recover it from whichever
        // operand is a sized integer.
        let cmp_repr = operand_cmp_repr(a, b, repr);
        let v = match op {
            BinOp::Add => Value::int(x.wrapping_add(y), repr),
            BinOp::Sub => Value::int(x.wrapping_sub(y), repr),
            BinOp::Mul => Value::int(x.wrapping_mul(y), repr),
            BinOp::Div => {
                if y == 0 {
                    Value::int(0, repr)
                } else if repr.signed && x == repr.min_value() && y == -1 {
                    // `type-MIN / -1` overflows the type. The inserted check traps
                    // this in safe builds; here (ReleaseFast, or an unsized
                    // operand) we return the defined wrapped value — `MIN` masked
                    // to the repr — rather than risk a Rust `i128::MIN / -1`
                    // arithmetic panic.
                    Value::int(repr.min_value(), repr)
                } else {
                    Value::int(x.wrapping_div(y), repr)
                }
            }
            BinOp::Rem => {
                // A zero divisor is trapped by the inserted check in safe builds;
                // in ReleaseFast we pick the defined sentinel 0 rather than panic
                // the Rust host. `wrapping_rem` already handles `MIN % -1`.
                if y == 0 {
                    Value::int(0, repr)
                } else {
                    Value::int(x.wrapping_rem(y), repr)
                }
            }
            // Bitwise `&`/`|`/`^` on two `bool` operands (e.g. the `@reduce(.And/
            // .Or/.Xor, @Vector(N,bool))` fold) yields a `bool`, not an int — the
            // native backend stores the fold's result in a 1-byte bool temp and so
            // prints `true`/`false`. Carry a `Value::Bool` here so the VM formats
            // it identically; mixed/int operands keep the integer bitwise result.
            BinOp::BitAnd if both_bool(a, b) => Value::Bool((x & y) != 0),
            BinOp::BitOr if both_bool(a, b) => Value::Bool((x | y) != 0),
            BinOp::BitXor if both_bool(a, b) => Value::Bool((x ^ y) != 0),
            BinOp::BitAnd => Value::int(x & y, repr),
            BinOp::BitOr => Value::int(x | y, repr),
            BinOp::BitXor => Value::int(x ^ y, repr),
            BinOp::Shl => {
                let sh = shift_amount(y, repr);
                Value::int(x.wrapping_shl(sh), repr)
            }
            BinOp::Shr => {
                let sh = shift_amount(y, repr);
                if repr.signed {
                    Value::int(x >> sh, repr)
                } else {
                    Value::int(((x as u128) >> sh) as i128, repr)
                }
            }
            BinOp::Eq => Value::Bool(self.values_eq(a, b)),
            BinOp::Ne => Value::Bool(!self.values_eq(a, b)),
            BinOp::Lt => Value::Bool(int_cmp(x, y, cmp_repr).is_lt()),
            BinOp::Le => Value::Bool(int_cmp(x, y, cmp_repr).is_le()),
            BinOp::Gt => Value::Bool(int_cmp(x, y, cmp_repr).is_gt()),
            BinOp::Ge => Value::Bool(int_cmp(x, y, cmp_repr).is_ge()),
        };
        Ok(v)
    }

    /// Evaluates a float binary op.
    fn float_binary(&self, op: BinOp, x: f64, y: f64) -> Value {
        match op {
            BinOp::Add => Value::Float(x + y),
            BinOp::Sub => Value::Float(x - y),
            BinOp::Mul => Value::Float(x * y),
            BinOp::Div => Value::Float(x / y),
            BinOp::Rem => Value::Float(x % y),
            BinOp::Eq => Value::Bool(x == y),
            BinOp::Ne => Value::Bool(x != y),
            BinOp::Lt => Value::Bool(x < y),
            BinOp::Le => Value::Bool(x <= y),
            BinOp::Gt => Value::Bool(x > y),
            BinOp::Ge => Value::Bool(x >= y),
            // Bitwise/shift on floats is nonsensical; the checker rejects it.
            _ => Value::Float(0.0),
        }
    }

    /// Structural equality for the value kinds equality is applied to (ints,
    /// bools, error tags, enums, strings).
    fn values_eq(&self, a: &Value, b: &Value) -> bool {
        // Null comparison: `x == null` lowers to `eq x, undef`, and a "no value"
        // result can be either `Optional(None)` or `Undef` (an opaque/`deferred`
        // optional, e.g. `sys.env.get(...)` returning absence). Treat the null
        // sentinels as interchangeable so `result == null` is true iff `result`
        // holds no value, and a present `Optional(Some(_))` is correctly unequal.
        let is_null = |v: &Value| matches!(v, Value::Optional(None) | Value::Undef(_));
        if is_null(a) || is_null(b) {
            return is_null(a) && is_null(b);
        }
        match (a, b) {
            (Value::Int { v: x, .. }, Value::Int { v: y, .. }) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::ErrVal(x), Value::ErrVal(y)) => x == y,
            (Value::Enum { tag: x, .. }, Value::Enum { tag: y, .. }) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Unit, Value::Unit) => true,
            // Mixed int/bool comparison (a discriminant compared to a literal).
            _ => match (a.as_i128(), b.as_i128()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        }
    }

    /// Evaluates a unary operation.
    fn eval_unary(&self, op: UnOp, a: &Value, repr: IntRepr) -> Result<Value, Halt> {
        let v = match op {
            UnOp::Neg => match a {
                Value::Float(f) => Value::Float(-f),
                _ => Value::int(a.as_i128().unwrap_or(0).wrapping_neg(), repr),
            },
            UnOp::BitNot => Value::int(!a.as_i128().unwrap_or(0), repr),
            UnOp::Not => Value::Bool(!a.as_bool().unwrap_or(false)),
        };
        Ok(v)
    }

    /// Evaluates a cast.
    fn eval_cast(&self, kind: CastKind, a: &Value, to: IntRepr, to_float: bool) -> Value {
        match kind {
            CastKind::IntToFloat => Value::Float(a.as_f64().unwrap_or(0.0)),
            CastKind::FloatToInt => Value::int(a.as_f64().unwrap_or(0.0) as i128, to),
            CastKind::PtrReinterpret => {
                // An integer `@bitCast` lowers to `PtrReinterpret`. Two's-complement
                // bit reinterpretation re-reprs the value to the *destination*
                // integer type, so `@bitCast(i8 -1) -> u8` is `255` and
                // `@bitCast(u8 255) -> i8` is `-1` — matching the native backend,
                // which normalizes to the target width/signedness. A genuine
                // pointer (or other non-integer) reinterpretation has no integer
                // value to re-repr, so it is passed through unchanged.
                match (a.as_i128(), to_float) {
                    (Some(bits), false) => Value::int(bits, to),
                    _ => a.clone(),
                }
            }
            CastKind::Widen | CastKind::IntNarrow => {
                if to_float {
                    Value::Float(a.as_f64().unwrap_or(0.0))
                } else {
                    Value::int(a.as_i128().unwrap_or(0), to)
                }
            }
        }
    }

    /// Packs a packed-struct `Value::Struct` into its backing integer
    /// (`@bitCast(packed) -> int`). Each field's value is masked to its bit width
    /// and OR'd in at its LSB-first bit offset, mirroring the native backend's
    /// single little-endian backing integer (spec §02). A field whose width is 0
    /// (a zero-width filler) contributes nothing.
    fn pack_struct(&self, a: &Value, fields: &[(u32, u32)], to: IntRepr) -> Result<Value, Halt> {
        let Value::Struct(vals) = a else {
            return Err(self.internal("@bitCast of a non-struct as a packed struct"));
        };
        let mut acc: u128 = 0;
        for (i, &(off, width)) in fields.iter().enumerate() {
            if width == 0 {
                continue;
            }
            let fv = vals.get(i).and_then(|v| v.as_i128()).unwrap_or(0);
            let mask: u128 = if width >= 128 {
                u128::MAX
            } else {
                (1u128 << width) - 1
            };
            acc |= ((fv as u128) & mask) << off;
        }
        Ok(Value::int(acc as i128, to))
    }

    /// Unpacks a backing integer into a packed-struct `Value::Struct`
    /// (`@bitCast(int) -> packed`). Each field is extracted by shifting down to
    /// its bit offset and re-repr'ing through its [`IntRepr`], which masks to the
    /// field width and sign-extends a signed field — exactly the native
    /// `load_packed_field` shift+mask (spec §02).
    fn unpack_struct(&self, a: &Value, fields: &[(u32, u32, IntRepr)]) -> Value {
        let bits = a.as_i128().unwrap_or(0) as u128;
        let out: Vec<Value> = fields
            .iter()
            .map(|&(off, width, repr)| {
                if width == 0 {
                    return Value::int(0, repr);
                }
                let raw = (bits >> off) as i128;
                // `Value::int` masks to `repr.width` and sign-extends, so the
                // extracted field carries the same value native loads.
                Value::int(raw, repr)
            })
            .collect();
        Value::Struct(std::rc::Rc::new(out))
    }

    // ---- projections ---------------------------------------------------

    /// Reads field `idx` of a struct/tuple (or an array element by index, for the
    /// rare field-into-array case).
    fn field_of(&self, base: &Value, idx: u32) -> Result<Value, Halt> {
        match base {
            Value::Struct(fields) | Value::Array(fields) => fields
                .get(idx as usize)
                .cloned()
                .ok_or_else(|| self.internal("field index out of range")),
            // A field read through a pointer-to-struct (a `self.field` receiver).
            Value::Ptr(p) => {
                let inner = self.heap.load(*p).map_err(|f| self.heap_panic(f))?;
                self.field_of(&inner, idx)
            }
            _ => Err(self.internal("field access on non-aggregate")),
        }
    }

    /// Reads element `i` of a slice/array/string/heap pointer.
    ///
    /// In **ReleaseFast** the lowerer strips the explicit bounds-`Check`, so this
    /// internal length test must match the native backend, which also emits no
    /// check: an out-of-bounds index is undefined behavior on both, and neither
    /// traps. The native backend reads adjacent stack (true garbage); the VM has no
    /// such storage, so it CLAMPS the index to the last valid element (the
    /// documented "ReleaseFast reads clamped" behavior) — a defined, non-trapping,
    /// non-panicking value. This keeps `native == VM` for the only-observable
    /// property of an OOB read in ReleaseFast (no trap, exit 0); the *value* of a
    /// genuine OOB read is UB and need not (cannot) match byte-for-byte. In Debug /
    /// ReleaseSafe the check is present and a real OOB still traps identically.
    fn index_of(&self, base: &Value, index: &Value) -> Result<Value, Halt> {
        let i = index.as_usize().unwrap_or(usize::MAX);
        match base {
            Value::Array(elems) => self
                .index_clamped(i, elems.len())
                .and_then(|j| elems.get(j).cloned())
                .ok_or_else(|| self.bounds_panic(i, elems.len())),
            // Indexing a `[]const u8` string yields its `i`-th byte as a `u8`.
            Value::Str(bytes) => self
                .index_clamped(i, bytes.len())
                .and_then(|j| bytes.get(j))
                .map(|&b| {
                    Value::int(
                        b as i128,
                        IntRepr {
                            width: 8,
                            signed: false,
                        },
                    )
                })
                .ok_or_else(|| self.bounds_panic(i, bytes.len())),
            Value::Slice { ptr, len } => match self.index_clamped(i, *len) {
                Some(j) => self
                    .heap
                    .load_index(*ptr, j)
                    .map_err(|f| self.heap_panic(f)),
                None => Err(self.bounds_panic(i, *len)),
            },
            Value::Ptr(p) => {
                // A pointer-to-array used as a slice (e.g. `&arr` passed to a
                // `[]T` parameter): index through the boxed array/allocation.
                match self.heap.load(*p) {
                    Ok(Value::Array(elems)) => self
                        .index_clamped(i, elems.len())
                        .and_then(|j| elems.get(j).cloned())
                        .ok_or_else(|| self.bounds_panic(i, elems.len())),
                    Ok(_) => {
                        let n = self.heap.len_of(*p).unwrap_or(0);
                        match self.index_clamped(i, n) {
                            Some(j) => self.heap.load_index(*p, j).map_err(|f| self.heap_panic(f)),
                            None => Err(self.bounds_panic(i, n)),
                        }
                    }
                    Err(f) => Err(self.heap_panic(f)),
                }
            }
            // An `undefined` array local read before any store: bounds-check
            // against its known length and yield a default-initialized element.
            Value::Undef(ty) => match self.prog.arena.get(*ty) {
                k2_types::Type::Array { len, elem } => {
                    let n = match len {
                        k2_types::ArrayLen::Known(n) => *n as usize,
                        _ => 0,
                    };
                    if self.index_clamped(i, n).is_none() {
                        return Err(self.bounds_panic(i, n));
                    }
                    Ok(default_value(&self.prog.arena, *elem))
                }
                _ => Err(self.internal("index of non-indexable value")),
            },
            _ => Err(self.internal("index of non-indexable value")),
        }
    }

    /// Resolves an index against a container of length `len` into the in-bounds
    /// element index to actually read/write. Returns `Some(i)` when `i < len`;
    /// when `i >= len` it returns `None` in checked builds (the caller raises a
    /// bounds panic) but, in **ReleaseFast** (`checks_off`), CLAMPS to `len - 1`
    /// so an OOB access does not trap — matching the native backend, which strips
    /// the bounds check entirely in ReleaseFast. An empty container has no valid
    /// element to clamp to, so it always yields `None` (no defined read exists).
    fn index_clamped(&self, i: usize, len: usize) -> Option<usize> {
        if i < len {
            Some(i)
        } else if self.checks_off && len > 0 {
            Some(len - 1)
        } else {
            None
        }
    }

    /// Follows a pointer.
    fn deref(&self, ptr: &Value) -> Result<Value, Halt> {
        match ptr {
            Value::Ptr(p) => self.heap.load(*p).map_err(|f| self.heap_panic(f)),
            // A deref of a non-pointer value (a by-value receiver passed directly)
            // just yields the value.
            other => Ok(other.clone()),
        }
    }

    /// Reads a slice's `.len` or `.ptr`.
    ///
    /// Reading the `.ptr` of a *by-value array* (the `arr[lo..hi]` sub-slice
    /// shape) boxes the array's elements into a heap `Many` cell and returns a
    /// real [`Ptr`] to them, so the resulting slice has a non-NULL data pointer
    /// and element load/store work. (Previously this returned `Unit`, which
    /// `make_slice` turned into a NULL-pointer slice that panicked on any access.)
    fn slice_meta(&mut self, base: &Value, which: SliceMeta) -> Result<Value, Halt> {
        match base {
            Value::Slice { ptr, len } => Ok(match which {
                SliceMeta::Len => Value::int(*len as i128, IntRepr::USIZE),
                SliceMeta::Ptr => Value::Ptr(*ptr),
            }),
            // A `[]const u8` string literal: `.len` is its byte length.
            Value::Str(bytes) => Ok(match which {
                SliceMeta::Len => Value::int(bytes.len() as i128, IntRepr::USIZE),
                SliceMeta::Ptr => Value::Unit,
            }),
            Value::Array(elems) => Ok(match which {
                SliceMeta::Len => Value::int(elems.len() as i128, IntRepr::USIZE),
                SliceMeta::Ptr => {
                    let ptr = self.heap.alloc_run((**elems).clone());
                    Value::Ptr(ptr)
                }
            }),
            Value::Ptr(p) => {
                // A pointer-to-array acting as a slice.
                match which {
                    SliceMeta::Len => {
                        let n = self.heap.len_of(*p).unwrap_or(0);
                        Ok(Value::int(n as i128, IntRepr::USIZE))
                    }
                    SliceMeta::Ptr => Ok(Value::Ptr(*p)),
                }
            }
            // An `undefined` array local: `var buf: [N]u8 = undefined`. Materialize
            // its default-initialized contents so `.len`/`.ptr` behave like a real
            // array (the canonical FixedBufferAllocator / bufPrint scratch shape).
            Value::Undef(ty)
                if matches!(self.prog.arena.get(*ty), k2_types::Type::Array { .. }) =>
            {
                let materialized = default_value(&self.prog.arena, *ty);
                self.slice_meta(&materialized, which)
            }
            _ => Err(self.internal("slice meta on non-slice")),
        }
    }

    /// Unwraps an optional's `Some` or an error union's `Ok` payload. A bare
    /// (already-unwrapped) value reads as itself, supporting the elided-`MakeSome`
    /// shapes (`_9: ?i32 = 7`).
    fn payload_of(&self, base: &Value) -> Value {
        match base {
            Value::Optional(Some(v)) => (**v).clone(),
            Value::Optional(None) => Value::Unit,
            Value::ErrOk(v) => (**v).clone(),
            other => other.clone(),
        }
    }

    /// Reads an optional/error-union/tagged-union discriminant. For an optional,
    /// `true` means `null`; for an error union, `true` means it holds an error;
    /// for a tagged union, the active variant index.
    fn discriminant(&self, base: &Value, kind: DiscrKind) -> Value {
        match kind {
            DiscrKind::Optional => {
                let is_null = matches!(base, Value::Optional(None) | Value::Undef(_));
                Value::Bool(is_null)
            }
            DiscrKind::ErrorUnion => {
                let is_err = matches!(base, Value::ErrVal(_));
                Value::Bool(is_err)
            }
            DiscrKind::Union => {
                let tag = match base {
                    Value::Enum { tag, .. } => *tag as i128,
                    Value::Int { v, .. } => *v,
                    _ => 0,
                };
                Value::int(tag, IntRepr::USIZE)
            }
        }
    }

    /// Builds a slice value from a pointer/array operand, an element offset (the
    /// sub-slice low bound `lo`), and a length. The offset is added to the base
    /// pointer's element offset so `base[lo..hi]` starts at element `lo`.
    fn make_slice(&self, ptr: &Value, offset: &Value, len: &Value) -> Value {
        let n = len.as_usize().unwrap_or(0);
        let off = offset.as_usize().unwrap_or(0);
        let with_offset = |p: Ptr| Ptr {
            cell: p.cell,
            offset: p.offset + off,
        };
        match ptr {
            Value::Ptr(p) => Value::Slice {
                ptr: with_offset(*p),
                len: n,
            },
            Value::Slice { ptr, .. } => Value::Slice {
                ptr: with_offset(*ptr),
                len: n,
            },
            _ => Value::Slice {
                ptr: Ptr::NULL,
                len: n,
            },
        }
    }

    // ---- stores & references ------------------------------------------

    /// Stores `value` into the place rooted at register `base` after walking
    /// `steps`. A `Deref` base writes straight through the pointer; field/index
    /// steps clone-on-write the aggregate and write it back to the base register.
    fn store_place(&mut self, base: Reg, steps: &[StoreStep], value: Value) -> Result<(), Halt> {
        if steps.is_empty() {
            self.set_cur(base, value);
            return Ok(());
        }
        // A single Deref step is the common `(*p) = v` case.
        if steps.len() == 1 {
            if let StoreStep::Deref = steps[0] {
                let pv = self.get(base);
                if let Value::Ptr(p) = pv {
                    self.heap.store(p, value).map_err(|f| self.heap_panic(f))?;
                    return Ok(());
                }
                // Non-pointer base: overwrite the register directly.
                self.set_cur(base, value);
                return Ok(());
            }
        }
        // General path: load the base value, mutate the addressed slot, store
        // back. We recursively descend, cloning aggregates on the way.
        let mut root = self.get(base);
        self.store_into(&mut root, steps, value)?;
        self.set_cur(base, root);
        Ok(())
    }

    /// Recursively writes `value` into `slot` along `steps`.
    fn store_into(
        &mut self,
        slot: &mut Value,
        steps: &[StoreStep],
        value: Value,
    ) -> Result<(), Halt> {
        let Some((first, rest)) = steps.split_first() else {
            *slot = value;
            return Ok(());
        };
        // An `undefined` aggregate local (`var buf: [N]u8 = undefined`) must be
        // materialized to its default-initialized contents before the first
        // field/index store, so writing `buf[i] = v` works instead of tripping the
        // internal "store on non-indexable" fault.
        if let Value::Undef(ty) = slot {
            if matches!(
                self.prog.arena.get(*ty),
                k2_types::Type::Array { .. } | k2_types::Type::Struct(_)
            ) {
                *slot = default_value(&self.prog.arena, *ty);
            }
        }
        match first {
            StoreStep::Deref => {
                // Write through a pointer slot.
                if let Value::Ptr(p) = slot {
                    if rest.is_empty() {
                        self.heap.store(*p, value).map_err(|f| self.heap_panic(f))?;
                    } else {
                        let mut inner = self.heap.load(*p).map_err(|f| self.heap_panic(f))?;
                        self.store_into(&mut inner, rest, value)?;
                        self.heap.store(*p, inner).map_err(|f| self.heap_panic(f))?;
                    }
                    Ok(())
                } else {
                    self.store_into(slot, rest, value)
                }
            }
            StoreStep::Field(idx) => {
                if let Value::Struct(fields) | Value::Array(fields) = slot {
                    let v = Rc::make_mut(fields);
                    if let Some(target) = v.get_mut(*idx as usize) {
                        self.store_into(target, rest, value)?;
                    }
                    Ok(())
                } else {
                    Err(self.internal("field store on non-aggregate"))
                }
            }
            StoreStep::Index(reg) => {
                // Mirror `index_of`: in ReleaseFast the bounds check is stripped, so
                // an OOB store is clamped to the last element (defined, no trap)
                // rather than panicking — matching the native backend.
                let i = self.get(*reg).as_usize().unwrap_or(usize::MAX);
                match slot {
                    Value::Array(elems) => {
                        let v = Rc::make_mut(elems);
                        let n = v.len();
                        let j = self
                            .index_clamped(i, n)
                            .ok_or_else(|| self.bounds_panic(i, n))?;
                        let target = &mut v[j];
                        self.store_into(target, rest, value)
                    }
                    Value::Slice { ptr, len } => {
                        let j = self
                            .index_clamped(i, *len)
                            .ok_or_else(|| self.bounds_panic(i, *len))?;
                        if rest.is_empty() {
                            self.heap
                                .store_index(*ptr, j, value)
                                .map_err(|f| self.heap_panic(f))
                        } else {
                            let mut inner = self
                                .heap
                                .load_index(*ptr, j)
                                .map_err(|f| self.heap_panic(f))?;
                            self.store_into(&mut inner, rest, value)?;
                            self.heap
                                .store_index(*ptr, j, inner)
                                .map_err(|f| self.heap_panic(f))
                        }
                    }
                    _ => Err(self.internal("index store on non-indexable")),
                }
            }
        }
    }

    /// Takes the address of a place. A bare local is boxed into a fresh heap cell
    /// holding its current value (the register is updated to that pointer so the
    /// local and the reference stay aliased). A projected place computes the
    /// interior pointer.
    fn take_ref(
        &mut self,
        base: Reg,
        base_is_local: bool,
        steps: &[StoreStep],
    ) -> Result<Value, Halt> {
        if base_is_local && steps.is_empty() {
            let cur = self.get(base);
            // If the local already holds a Ptr (an address-taken slot), reuse it.
            if let Value::Ptr(p) = cur {
                return Ok(Value::Ptr(p));
            }
            // An `= undefined` aggregate (`var storage: [N]u8 = undefined`) holds a
            // bare `Value::Undef(array_ty)`; boxing that as-is gives a cell whose
            // `len_of` is 1, so `FixedBufferAllocator.init(&storage)` would report a
            // capacity of 1 (and element stores would have nowhere to land).
            // Materialize it to a real default-initialized array/struct first, so
            // the boxed cell has the right length and indexable slots. (Mirrors the
            // `slice_meta`/`store_into` undef-materialization at the use sites.)
            let cur = self.materialize_undef_aggregate(cur);
            let ptr = self.heap.alloc_one(cur);
            self.set_cur(base, Value::Ptr(ptr));
            Ok(Value::Ptr(ptr))
        } else {
            // Projected reference: resolve to a heap pointer if the base is a
            // pointer or slice; otherwise box the projected value.
            let bv = self.get(base);
            self.ref_through(&bv, steps)
        }
    }

    /// Materializes an `= undefined` array/struct value to its default-initialized
    /// contents, so a boxed-then-indexed aggregate behaves like a real one. Any
    /// other value (including a scalar `undefined`) is returned unchanged.
    fn materialize_undef_aggregate(&self, v: Value) -> Value {
        if let Value::Undef(ty) = v {
            if matches!(
                self.prog.arena.get(ty),
                k2_types::Type::Array { .. } | k2_types::Type::Struct(_)
            ) {
                return default_value(&self.prog.arena, ty);
            }
            return Value::Undef(ty);
        }
        v
    }

    /// Resolves a reference through a projection chain.
    fn ref_through(&mut self, base: &Value, steps: &[StoreStep]) -> Result<Value, Halt> {
        match steps.split_first() {
            None => {
                // Box the value so callers get a stable pointer. Materialize an
                // `= undefined` aggregate first (see `take_ref`).
                let boxed = self.materialize_undef_aggregate(base.clone());
                let ptr = self.heap.alloc_one(boxed);
                Ok(Value::Ptr(ptr))
            }
            Some((StoreStep::Deref, rest)) => {
                if let Value::Ptr(p) = base {
                    if rest.is_empty() {
                        Ok(Value::Ptr(*p))
                    } else {
                        let inner = self.heap.load(*p).map_err(|f| self.heap_panic(f))?;
                        self.ref_through(&inner, rest)
                    }
                } else {
                    self.ref_through(base, rest)
                }
            }
            Some((StoreStep::Index(reg), rest)) => {
                let i = self.get(*reg).as_usize().unwrap_or(0);
                match base {
                    Value::Slice { ptr, .. } if rest.is_empty() => Ok(Value::Ptr(Ptr {
                        cell: ptr.cell,
                        offset: ptr.offset + i,
                    })),
                    _ => {
                        let elem = self.index_of(base, &Value::int(i as i128, IntRepr::USIZE))?;
                        self.ref_through(&elem, rest)
                    }
                }
            }
            Some((StoreStep::Field(idx), rest)) => {
                let f = self.field_of(base, *idx)?;
                self.ref_through(&f, rest)
            }
        }
    }

    // ---- intrinsics ----------------------------------------------------

    /// Dispatches a resolved intrinsic, returning the result value *or* a suspend/
    /// yield signal for the cooperative scheduler. The concurrency floor is handled
    /// here (some of it blocking); every other intrinsic delegates to
    /// [`Vm::dispatch_intrinsic_value`] and wraps its value.
    fn dispatch_intrinsic(
        &mut self,
        id: &IntrinsicId,
        recv: Option<Value>,
        args: &[Value],
    ) -> Result<IntrinsicOutcome, Halt> {
        // A concurrency method reached as `value.<method>` (a deferred receiver —
        // e.g. `c.mu.lock()` where the field access lost the concrete `Mutex` type)
        // carries the handle in the RECEIVER, not the operands. Unify by prepending
        // the (unwrapped) receiver handle so every scheduler handler finds it the
        // same way whether reached via the std method body or this deferred path.
        if is_concurrency_intrinsic(id) {
            if let Some(r) = &recv {
                let mut merged = Vec::with_capacity(args.len() + 1);
                merged.push(self.handle_of_receiver(r));
                merged.extend_from_slice(args);
                return self.dispatch_concurrency(id, &merged);
            }
            return self.dispatch_concurrency(id, args);
        }
        // The *Build capability floor records into the build graph (no I/O). The
        // `*Artifact`/`*Step` methods carry their handle id in the RECEIVER (a
        // `{ id }` struct, or a pointer to one); prepend it so the recorder reads
        // it uniformly, exactly like the concurrency floor.
        if is_build_intrinsic(id) {
            // The `Target` field reads index the receiver struct directly.
            if let Some(field) = build_target_field(id) {
                let v = match &recv {
                    Some(Value::Struct(fields)) => {
                        fields.get(field).cloned().unwrap_or(Value::Unit)
                    }
                    _ => Value::Unit,
                };
                return Ok(IntrinsicOutcome::Value(v));
            }
            if needs_handle_receiver(id) {
                if let Some(r) = &recv {
                    let mut merged = Vec::with_capacity(args.len() + 1);
                    merged.push(self.handle_of_receiver(r));
                    merged.extend_from_slice(args);
                    return self
                        .dispatch_build(id, &merged)
                        .map(IntrinsicOutcome::Value);
                }
            }
            return self.dispatch_build(id, args).map(IntrinsicOutcome::Value);
        }
        // Every other intrinsic is non-blocking: compute its value and wrap it.
        self.dispatch_intrinsic_value(id, recv, args)
            .map(IntrinsicOutcome::Value)
    }

    /// Materializes any heap-backed byte-`Slice` format argument into a `Str` so
    /// the (heap-blind) format engine renders it correctly under `{s}`/`{}`.
    ///
    /// A `[]const u8` / `[]u8` built at runtime (e.g. `buf[0..n]`, a container's
    /// owned bytes, `Big.toDecimal`'s digit slice) reaches the formatter as a
    /// fat `Value::Slice { ptr, len }` whose bytes live in the managed heap; the
    /// pure `fmt::format_into` cannot read them. This walks the argument tuple
    /// and, for each slice whose elements are all 8-bit integers (exactly a byte
    /// slice), reads those bytes here — where the heap IS reachable — and swaps
    /// in an owned `Value::Str`. Non-byte slices and every other value are left
    /// untouched, so this only ever fixes the previously-`<int>` byte-slice case.
    fn materialize_str_slice_args(&self, args: &mut [Value]) {
        for a in args.iter_mut() {
            if let Value::Slice { ptr, len } = a {
                let (ptr, len) = (*ptr, *len);
                let mut bytes = Vec::with_capacity(len);
                let mut all_u8 = true;
                for i in 0..len {
                    match self.heap.load_index(ptr, i) {
                        // A byte element is either an explicit `u8` (width 8) or a
                        // still-untyped integer literal (width 0, the comptime_int a
                        // `buf[i] = 104` store leaves) whose value fits in a byte. A
                        // wider element (width 16/32/64) is NOT a byte slice and is
                        // left as-is, so only true byte runs become strings.
                        Ok(Value::Int { v, repr })
                            if repr.width == 8 || (repr.width == 0 && (0..=255).contains(&v)) =>
                        {
                            bytes.push(v as u8)
                        }
                        _ => {
                            all_u8 = false;
                            break;
                        }
                    }
                }
                if all_u8 {
                    *a = Value::Str(Rc::new(bytes));
                }
            }
        }
    }

    /// Reads a `[]const u8` argument as a Rust `String`, from either a `Str`
    /// literal value or a heap-backed `Slice` (e.g. `b.fmt(...)`'s `@bufPrint`
    /// result). A non-string value yields the empty string.
    fn read_bytes(&self, v: Option<&Value>) -> String {
        match v {
            Some(Value::Str(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(Value::Slice { ptr, len }) => {
                let mut out = Vec::with_capacity(*len);
                for i in 0..*len {
                    match self.heap.load_index(*ptr, i) {
                        Ok(Value::Int { v, .. }) => out.push(v as u8),
                        _ => break,
                    }
                }
                String::from_utf8_lossy(&out).into_owned()
            }
            _ => String::new(),
        }
    }

    /// Reads a build handle id from a value: a bare integer, a `{ id }` handle
    /// struct (Artifact/Step/Module), or a pointer to one (`&run_exe.step`). This
    /// is how `addRunArtifact(exe)`, `installArtifact(a)`, `addModule(.., mod)`,
    /// and `dependOn(&other.step)` read the id out of their handle operand.
    fn build_handle_id(&self, v: Option<&Value>) -> u32 {
        match v {
            Some(Value::Int { v, .. }) => *v as u32,
            Some(Value::Struct(fields)) => {
                fields.first().and_then(|f| f.as_usize()).unwrap_or(0) as u32
            }
            Some(Value::Ptr(p)) => match self.heap.load(*p) {
                Ok(inner) => self.build_handle_id(Some(&inner)),
                Err(_) => 0,
            },
            _ => 0,
        }
    }

    /// Dispatches one `*Build` capability intrinsic. Every variant is a pure
    /// recorder: it reads driver-seeded inputs and/or pushes a node into the build
    /// graph, performing no I/O and no real allocation (the comptime sandbox). The
    /// graph is stored in creation order, so the result — and the lockfile derived
    /// from it — is deterministic.
    fn dispatch_build(&mut self, id: &IntrinsicId, args: &[Value]) -> Result<Value, Halt> {
        // Every build intrinsic needs the recording context; its absence means a
        // `@build*` reached an ordinary `run`/`test`, which is a clean panic.
        if self.build.is_none() {
            return Err(Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some("build intrinsic reached outside `k2 build`".to_string()),
            }));
        }
        match id {
            IntrinsicId::BuildStdTarget => Ok(self.build_target_value()),
            IntrinsicId::BuildStdOptimize => {
                let idx = self.build.as_ref().unwrap().graph.optimize.index();
                Ok(Value::Enum {
                    tag: idx as u32,
                    payload: Box::new(Value::Unit),
                })
            }
            IntrinsicId::BuildOption => Ok(self.build_option(args)),
            IntrinsicId::BuildAddLibrary => {
                Ok(self.build_add_artifact(ArtifactKind::Library, args))
            }
            IntrinsicId::BuildAddExecutable => {
                Ok(self.build_add_artifact(ArtifactKind::Executable, args))
            }
            IntrinsicId::BuildAddTest => Ok(self.build_add_artifact(ArtifactKind::Test, args)),
            IntrinsicId::BuildArtifactOption => {
                self.build_artifact_option(args);
                Ok(Value::Unit)
            }
            IntrinsicId::BuildArtifactModule => {
                self.build_artifact_module(args);
                Ok(Value::Unit)
            }
            IntrinsicId::BuildArtifactModuleSelf => Ok(self.build_artifact_module_self(args)),
            IntrinsicId::BuildArtifactStep => Ok(self.build_artifact_step(args)),
            IntrinsicId::BuildArtifactForwardArgs => {
                self.build_artifact_forward_args(args);
                Ok(Value::Unit)
            }
            IntrinsicId::BuildAddRun => Ok(self.build_add_run(args)),
            IntrinsicId::BuildInstall => {
                self.build_install(args);
                Ok(Value::Unit)
            }
            IntrinsicId::BuildStep => Ok(self.build_step(args)),
            IntrinsicId::BuildStepDependOn => {
                self.build_step_depend_on(args);
                Ok(Value::Unit)
            }
            // `b.path(rel)` is identity: return the path string operand verbatim.
            IntrinsicId::BuildPath => Ok(args
                .iter()
                .find(|a| matches!(a, Value::Str(_) | Value::Slice { .. }))
                .cloned()
                .unwrap_or(Value::Unit)),
            // `b.fmt(template, args)` formats into a fresh string via the shared
            // engine (no build-time I/O, no allocator).
            IntrinsicId::BuildFmt => self.build_fmt(args),
            _ => Err(self.internal("non-build intrinsic in build dispatcher")),
        }
    }

    /// Builds the `Target` struct value `@buildStdTarget()` returns: a 3-field
    /// `{ arch, os, abi }` of enum values, so a build script can `switch
    /// (target.os)`.
    fn build_target_value(&self) -> Value {
        let t = &self.build.as_ref().unwrap().graph.target;
        let mk = |idx: i128| Value::Enum {
            tag: idx as u32,
            payload: Box::new(Value::Unit),
        };
        Value::Struct(Rc::new(vec![
            mk(t.arch_index()),
            mk(t.os_index()),
            mk(t.abi_index()),
        ]))
    }

    /// `b.fmt(template, args)`: format `args` into a fresh string via the shared
    /// format engine, returning a `[]const u8` (`Value::Str`). No build-time I/O
    /// and no allocator — the bytes live in the returned value.
    fn build_fmt(&mut self, args: &[Value]) -> Result<Value, Halt> {
        let template = args
            .iter()
            .find(|a| matches!(a, Value::Str(_) | Value::Slice { .. }))
            .cloned();
        let fmt_bytes = match &template {
            Some(Value::Str(b)) => b.as_ref().clone(),
            Some(v @ Value::Slice { .. }) => self.read_bytes(Some(v)).into_bytes(),
            _ => Vec::new(),
        };
        // The argument tuple is the first struct/array operand after the template.
        let arg_values: Vec<Value> = args
            .iter()
            .find_map(|a| match a {
                Value::Struct(fields) | Value::Array(fields) => Some(fields.as_ref().clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut buf = Vec::new();
        format_into(&mut buf, &fmt_bytes, &arg_values).map_err(|e| {
            Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some(e),
            })
        })?;
        Ok(Value::Str(Rc::new(buf)))
    }

    /// `@buildOption(T, name, desc)`: look up `name` in the `-D` map, record the
    /// declared option for `--help`/lock, and return `?T` (`null` → `orelse` when
    /// absent). The first operand is the declared option type `T`, carried as an
    /// `undef` of the CONCRETE type (the MIR lowerer gives a type-denoting argument
    /// its denoted type), so the VM honors the DECLARED kind (`bool`/`int`/`string`)
    /// rather than guessing it from the `-D` value string. The two string operands
    /// are `name` then `desc`.
    fn build_option(&mut self, args: &[Value]) -> Value {
        let strs: Vec<String> = args
            .iter()
            .filter(|a| matches!(a, Value::Str(_) | Value::Slice { .. }))
            .map(|a| self.read_bytes(Some(a)))
            .collect();
        let name = strs.first().cloned().unwrap_or_default();
        let desc = strs.get(1).cloned().unwrap_or_default();
        let kind = option_kind_of(type_carrier(args), &self.prog.arena);
        let (kind_word, present, value) = self.option_for_kind(kind, &name);
        let ctx = self.build.as_mut().unwrap();
        // Record the declared option once (first declaration wins).
        if !ctx.graph.options.iter().any(|o| o.name == name) {
            ctx.graph.options.push(DeclaredOption {
                name: name.clone(),
                kind: kind_word.to_string(),
                desc,
            });
        }
        if present {
            Value::Optional(Some(Box::new(value)))
        } else {
            Value::Optional(None)
        }
    }

    /// Computes `(kind_word, present, value)` for a build option of the given
    /// declared [`OptionKind`], coercing the `-D` value string to that kind:
    ///
    /// * `Bool` accepts `true`/`1`/`yes`/`on` (and a bare `-Dflag`) as true and
    ///   ANY other value — `0`/`false`/`no`/`off`/anything — as false, so a bool
    ///   option NEVER breaks the build for a non-`true`/`false` value.
    /// * `Int` parses the value (a non-numeric value coerces to `0`).
    /// * `String` keeps the value verbatim, so a numeric-looking string stays a
    ///   string (no more `.len`-on-an-int build-script panic).
    ///
    /// An absent option returns `(kind, false, _)` so the caller's `orelse`
    /// supplies the default.
    fn option_for_kind(&self, kind: OptionKind, name: &str) -> (&'static str, bool, Value) {
        let raw = self
            .build
            .as_ref()
            .unwrap()
            .inputs
            .dopts
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone());
        match kind {
            OptionKind::Bool => {
                let present = raw.is_some();
                let b = matches!(
                    raw.as_deref().map(str::trim).map(str::to_ascii_lowercase),
                    Some(ref s) if matches!(s.as_str(), "true" | "1" | "yes" | "on" | "")
                );
                ("bool", present, Value::Bool(b))
            }
            OptionKind::Int => match raw {
                Some(s) => (
                    "int",
                    true,
                    Value::int(s.trim().parse().unwrap_or(0), IntRepr::COMPTIME),
                ),
                None => ("int", false, Value::Unit),
            },
            OptionKind::String => match raw {
                Some(s) => ("string", true, Value::Str(Rc::new(s.into_bytes()))),
                None => ("string", false, Value::Unit),
            },
        }
    }

    /// `b.addLibrary/addExecutable/addTest(cfg)`: push the artifact node and
    /// return its `u32` id. The `cfg` is the anonymous config struct, read
    /// positionally — field 0 = `.name`, field 1 = `.root_source` (the example's
    /// canonical field order). Each artifact gets an embedded step id so
    /// `&exe.step` / `installArtifact` work.
    fn build_add_artifact(&mut self, kind: ArtifactKind, args: &[Value]) -> Value {
        // The first struct/array operand is the config aggregate.
        let cfg = args.iter().find_map(|a| match a {
            Value::Struct(fields) | Value::Array(fields) => Some(fields.clone()),
            _ => None,
        });
        let (name, root_source) = match cfg {
            Some(fields) => {
                let name = self.read_bytes(fields.first());
                let root = fields.get(1).map(|f| self.read_bytes(Some(f)));
                (name, root)
            }
            None => {
                // Fall back to bare positional string operands.
                let strs: Vec<String> = args
                    .iter()
                    .filter(|a| matches!(a, Value::Str(_) | Value::Slice { .. }))
                    .map(|a| self.read_bytes(Some(a)))
                    .collect();
                (
                    strs.first().cloned().unwrap_or_default(),
                    strs.get(1).cloned(),
                )
            }
        };
        let root_source = root_source.filter(|s| !s.is_empty());
        let ctx = self.build.as_mut().unwrap();
        let id = ctx.graph.artifacts.len() as u32;
        let step_id = ctx.graph.steps.len() as u32;
        ctx.graph.steps.push(StepNode {
            id: step_id,
            name: None,
            desc: format!("build {name}"),
            deps: Vec::new(),
        });
        ctx.graph.artifacts.push(Artifact {
            id,
            kind,
            name,
            root_source,
            modules: Vec::new(),
            options: Vec::new(),
            exe_id: None,
            forward_args: false,
            step_id,
        });
        Value::int(id as i128, IntRepr::USIZE)
    }

    /// `a.addOption(T, name, value)`: append a comptime-known build option to
    /// artifact `a`'s options table. The receiver supplies `args[0]` = the
    /// artifact id; the remaining operands are the type carrier, the name, and the
    /// value (the type carrier is skipped — the value itself carries its shape).
    fn build_artifact_option(&mut self, args: &[Value]) {
        let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
        // After the id, drop the `Undef` type carrier; the remaining operands are
        // the name (first string) then the value (bool/int, or a second string).
        let rest: Vec<&Value> = args
            .iter()
            .skip(1)
            .filter(|a| !matches!(a, Value::Undef(_)))
            .collect();
        let name = rest
            .first()
            .map(|a| self.read_bytes(Some(a)))
            .unwrap_or_default();
        let val = self.opt_val_of(rest.get(1).copied());
        if let Some(ctx) = self.build.as_mut() {
            if let Some(a) = ctx.graph.artifacts.get_mut(id) {
                // Last write wins for a repeated option name.
                if let Some(slot) = a.options.iter_mut().find(|(n, _)| *n == name) {
                    slot.1 = val;
                } else {
                    a.options.push((name, val));
                }
            }
        }
    }

    /// Captures a build-option value (`bool`/`[]const u8`/int) for the options
    /// table.
    fn opt_val_of(&self, v: Option<&Value>) -> BuildOptVal {
        match v {
            Some(Value::Bool(b)) => BuildOptVal::Bool(*b),
            Some(Value::Str(_)) | Some(Value::Slice { .. }) => BuildOptVal::Str(self.read_bytes(v)),
            Some(Value::Int { v, .. }) => BuildOptVal::Int(*v),
            _ => BuildOptVal::Bool(false),
        }
    }

    /// `a.addModule(name, mod)`: record that import name `name` inside artifact
    /// `a` resolves to module `mod`. The receiver supplies `args[0]` = the
    /// artifact id; `args[1]` = the name; `args[2]` = the `Module` handle struct.
    fn build_artifact_module(&mut self, args: &[Value]) {
        let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
        let name = self.read_bytes(args.get(1));
        let mod_id = self.build_handle_id(args.get(2));
        if let Some(ctx) = self.build.as_mut() {
            if let Some(a) = ctx.graph.artifacts.get_mut(id) {
                if !a.modules.iter().any(|(n, _)| *n == name) {
                    a.modules.push((name, mod_id));
                }
            }
        }
    }

    /// `@buildArtifactModuleSelf(id)`: mint a `Module` value (a `{ id }` struct)
    /// that exposes artifact `id`'s root source, so `lib.module()` is reusable.
    fn build_artifact_module_self(&mut self, args: &[Value]) -> Value {
        let artifact_id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
        let ctx = self.build.as_mut().unwrap();
        // Reuse an existing module node for this artifact if one was minted.
        let mod_id = if let Some(m) = ctx
            .graph
            .module_nodes
            .iter()
            .find(|m| m.artifact_id == artifact_id)
        {
            m.id
        } else {
            let mod_id = ctx.graph.module_nodes.len() as u32;
            ctx.graph.module_nodes.push(ModuleNode {
                id: mod_id,
                artifact_id,
            });
            mod_id
        };
        Value::Struct(Rc::new(vec![Value::int(mod_id as i128, IntRepr::USIZE)]))
    }

    /// `artifact.step` (field read): return a `Step` handle `{ step_id }` for the
    /// artifact's embedded step, so `&run_exe.step` is a real step to depend on.
    fn build_artifact_step(&self, args: &[Value]) -> Value {
        let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
        let step_id = self
            .build
            .as_ref()
            .unwrap()
            .graph
            .artifacts
            .get(id)
            .map(|a| a.step_id)
            .unwrap_or(0);
        Value::Struct(Rc::new(vec![Value::int(step_id as i128, IntRepr::USIZE)]))
    }

    /// `@buildArtifactForwardArgs(id)`: flag a run-artifact to receive `--`-args.
    fn build_artifact_forward_args(&mut self, args: &[Value]) {
        let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
        if let Some(ctx) = self.build.as_mut() {
            if let Some(a) = ctx.graph.artifacts.get_mut(id) {
                a.forward_args = true;
            }
        }
    }

    /// `b.addRunArtifact(exe)`: push a `Run` artifact wrapping `exe`, with its own
    /// embedded step, and return its id. `exe` is an `Artifact` handle struct.
    fn build_add_run(&mut self, args: &[Value]) -> Value {
        let exe_id = self.build_handle_id(args.first());
        let ctx = self.build.as_mut().unwrap();
        let id = ctx.graph.artifacts.len() as u32;
        let step_id = ctx.graph.steps.len() as u32;
        ctx.graph.steps.push(StepNode {
            id: step_id,
            name: None,
            desc: format!("run artifact {exe_id}"),
            deps: Vec::new(),
        });
        ctx.graph.artifacts.push(Artifact {
            id,
            kind: ArtifactKind::Run,
            name: format!("run{id}"),
            root_source: None,
            modules: Vec::new(),
            options: Vec::new(),
            exe_id: Some(exe_id),
            forward_args: false,
            step_id,
        });
        Value::int(id as i128, IntRepr::USIZE)
    }

    /// `b.installArtifact(a)`: add artifact `a` to the install step's deps. `a`
    /// is an `Artifact` handle struct.
    fn build_install(&mut self, args: &[Value]) {
        let id = self.build_handle_id(args.first());
        if let Some(ctx) = self.build.as_mut() {
            if !ctx.graph.install.contains(&id) {
                ctx.graph.install.push(id);
            }
        }
    }

    /// `@buildStep(name, desc)`: push a named step and return its id.
    fn build_step(&mut self, args: &[Value]) -> Value {
        let name = self.read_bytes(args.first());
        let desc = self.read_bytes(args.get(1));
        let ctx = self.build.as_mut().unwrap();
        let id = ctx.graph.steps.len() as u32;
        ctx.graph.steps.push(StepNode {
            id,
            name: Some(name),
            desc,
            deps: Vec::new(),
        });
        Value::Struct(Rc::new(vec![Value::int(id as i128, IntRepr::USIZE)]))
    }

    /// `step.dependOn(&other.step)`: add a DAG edge. The receiver supplies
    /// `args[0]` = this step's id; `args[1]` = the depended-on step (a `&step`
    /// pointer to a `Step` handle struct).
    fn build_step_depend_on(&mut self, args: &[Value]) {
        let step_id = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
        let dep_id = self.build_handle_id(args.get(1));
        if let Some(ctx) = self.build.as_mut() {
            if let Some(s) = ctx.graph.steps.get_mut(step_id) {
                if !s.deps.contains(&dep_id) {
                    s.deps.push(dep_id);
                }
            }
        }
    }

    /// Dispatches one concurrency / scheduler intrinsic. `args[0]` (when the op
    /// takes a handle) is the scheduler-object handle.
    fn dispatch_concurrency(
        &mut self,
        id: &IntrinsicId,
        args: &[Value],
    ) -> Result<IntrinsicOutcome, Halt> {
        match id {
            IntrinsicId::SchedSpawn => self.sched_spawn(args).map(IntrinsicOutcome::Value),
            IntrinsicId::SchedYield => Ok(IntrinsicOutcome::Yield),
            IntrinsicId::SchedAwait => Ok(self.sched_await(args)),
            IntrinsicId::SchedRun => Ok(self.sched_run()),
            IntrinsicId::ChanMake => Ok(IntrinsicOutcome::Value(self.chan_make(args))),
            IntrinsicId::ChanSend => Ok(self.chan_send(args)),
            IntrinsicId::ChanRecv => Ok(self.chan_recv(args)),
            IntrinsicId::ChanClose => {
                self.chan_close(args);
                Ok(IntrinsicOutcome::Value(Value::Unit))
            }
            IntrinsicId::ChanLen => Ok(IntrinsicOutcome::Value(self.chan_len(args))),
            IntrinsicId::MutexMake => Ok(IntrinsicOutcome::Value(Value::Sched {
                kind: SchedKind::Mutex,
                id: self.sched.make_mutex(),
            })),
            IntrinsicId::MutexLock => Ok(self.mutex_lock(args)),
            IntrinsicId::MutexUnlock => {
                self.mutex_unlock(args);
                Ok(IntrinsicOutcome::Value(Value::Unit))
            }
            IntrinsicId::AtomicMake => Ok(IntrinsicOutcome::Value(self.atomic_make(args))),
            IntrinsicId::AtomicLoad => Ok(IntrinsicOutcome::Value(self.atomic_load(args))),
            IntrinsicId::AtomicStore => {
                self.atomic_store(args);
                Ok(IntrinsicOutcome::Value(Value::Unit))
            }
            IntrinsicId::AtomicFetchAdd => Ok(IntrinsicOutcome::Value(self.atomic_fetch_add(args))),
            IntrinsicId::AtomicSwap => Ok(IntrinsicOutcome::Value(self.atomic_swap(args))),
            IntrinsicId::AtomicCas => Ok(IntrinsicOutcome::Value(self.atomic_cas(args))),
            IntrinsicId::WgMake => Ok(IntrinsicOutcome::Value(Value::Sched {
                kind: SchedKind::WaitGroup,
                id: self.sched.make_waitgroup(),
            })),
            IntrinsicId::WgAdd => {
                self.wg_add(args);
                Ok(IntrinsicOutcome::Value(Value::Unit))
            }
            IntrinsicId::WgDone => {
                self.wg_done(args);
                Ok(IntrinsicOutcome::Value(Value::Unit))
            }
            IntrinsicId::WgWait => Ok(self.wg_wait(args)),
            _ => Err(self.internal("non-concurrency intrinsic in scheduler dispatcher")),
        }
    }

    /// Dispatches every non-concurrency intrinsic (the v0.10 floor), returning its
    /// result value. None of these suspend.
    fn dispatch_intrinsic_value(
        &mut self,
        id: &IntrinsicId,
        recv: Option<Value>,
        args: &[Value],
    ) -> Result<Value, Halt> {
        match id {
            IntrinsicId::StdoutWriter => Ok(Value::Cap(Capability::StdoutWriter)),
            IntrinsicId::StderrWriter => Ok(Value::Cap(Capability::StderrWriter)),
            IntrinsicId::IoCap => Ok(Value::Cap(Capability::Io)),
            // `sys.heap` is the default allocator (handle id 0).
            IntrinsicId::HeapCap => Ok(Value::Cap(Capability::Allocator(0))),
            IntrinsicId::Print => self.intrinsic_print(recv, args),
            // The `Allocator`-value method floor: `alloc.create/alloc/free/destroy`
            // reached as member calls on an `Allocator` value. The handle id is on
            // the receiver (`Capability::Allocator(id)`, or the bare `sys.heap`).
            IntrinsicId::Create => {
                let id = alloc_id_of(&recv);
                let ty = type_carrier(args).unwrap_or_else(|| self.prog.arena.t_void());
                self.alloc_create(id, ty)
            }
            IntrinsicId::Alloc => {
                let id = alloc_id_of(&recv);
                let ty = type_carrier(args).unwrap_or_else(|| self.prog.arena.t_void());
                let n = args.iter().find_map(|a| a.as_usize()).unwrap_or(0);
                self.alloc_many(id, ty, n)
            }
            IntrinsicId::Destroy => {
                let id = alloc_id_of(&recv);
                self.alloc_free(id, args.first())
            }
            IntrinsicId::Free => {
                let id = alloc_id_of(&recv);
                self.alloc_free(id, args.first())
            }

            // ---- The std allocator floor (handle-based) ------------------
            IntrinsicId::AllocId => {
                let kind = args.first().and_then(|v| v.as_usize()).unwrap_or(0);
                let buf = args.get(2).cloned();
                Ok(Value::int(
                    self.register_allocator(kind, buf) as i128,
                    IntRepr::USIZE,
                ))
            }
            IntrinsicId::AllocHandle => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                Ok(Value::Cap(Capability::Allocator(id)))
            }
            IntrinsicId::AllocRaw => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                let ty = type_carrier(args).unwrap_or_else(|| self.prog.arena.t_void());
                // The element count is the last non-type, non-id integer operand.
                let n = args
                    .iter()
                    .skip(1)
                    .filter(|a| !matches!(a, Value::Undef(_)))
                    .find_map(|a| a.as_usize())
                    .unwrap_or(0);
                self.alloc_many(id, ty, n)
            }
            IntrinsicId::CreateRaw => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                let ty = type_carrier(args).unwrap_or_else(|| self.prog.arena.t_void());
                self.alloc_create(id, ty)
            }
            IntrinsicId::ReallocRaw => {
                let id = alloc_id_for_realloc(&recv, args);
                self.alloc_realloc(id, args)
            }
            IntrinsicId::FreeRaw => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                self.alloc_free(id, args.get(1))
            }
            IntrinsicId::DestroyRaw => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                self.alloc_free(id, args.get(1))
            }
            IntrinsicId::ArenaDeinit => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                self.arena_deinit(id);
                Ok(Value::Unit)
            }
            IntrinsicId::GpaDeinit => {
                let id = args.first().and_then(|v| v.as_usize()).unwrap_or(0) as u32;
                Ok(Value::Bool(self.gpa_deinit(id)))
            }

            // ---- The *System capability floor ----------------------------
            IntrinsicId::ClockNow => {
                // `which`: 0 monotonic, 1 wall. Both read the deterministic VM
                // monotonic counter so test output is reproducible.
                Ok(Value::int(self.clock_nanos as i128, IntRepr::USIZE))
            }
            IntrinsicId::ClockSleep => {
                let ns = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
                self.clock_nanos = self.clock_nanos.saturating_add(ns.max(0) as u64);
                Ok(Value::Unit)
            }
            IntrinsicId::RandomBytes => {
                self.random_bytes(args.first());
                Ok(Value::Unit)
            }
            IntrinsicId::RandomInt => Ok(Value::int(self.next_random() as i128, IntRepr::USIZE)),
            IntrinsicId::EnvGet => {
                // Offline-safe: no host env is consulted; every lookup is absent.
                Ok(Value::Optional(None))
            }
            IntrinsicId::BufPrint => self.intrinsic_buf_print(args),
            IntrinsicId::SliceLen => match recv {
                Some(Value::Slice { len, .. }) => Ok(Value::int(len as i128, IntRepr::USIZE)),
                _ => Ok(Value::int(0, IntRepr::USIZE)),
            },
            IntrinsicId::SlicePtr => match recv {
                Some(Value::Slice { ptr, .. }) => Ok(Value::Ptr(ptr)),
                _ => Ok(Value::Ptr(Ptr::NULL)),
            },
            IntrinsicId::ErrorName => {
                let name = match args.first() {
                    Some(v) => self.error_name_of(v),
                    None => "Unknown".to_string(),
                };
                Ok(Value::Str(Rc::new(name.into_bytes())))
            }
            IntrinsicId::TypeName => {
                let name = match args.first() {
                    Some(Value::Undef(ty)) => self.prog.arena.fmt(*ty),
                    _ => "unknown".to_string(),
                };
                Ok(Value::Str(Rc::new(name.into_bytes())))
            }
            IntrinsicId::NoAddOverflow => Ok(self.no_overflow(args, ArithKind::Add)),
            IntrinsicId::NoSubOverflow => Ok(self.no_overflow(args, ArithKind::Sub)),
            IntrinsicId::NoMulOverflow => Ok(self.no_overflow(args, ArithKind::Mul)),
            IntrinsicId::NoDivOverflow => Ok(self.no_div_overflow(args)),
            IntrinsicId::NoNegOverflow => Ok(self.no_neg_overflow(args)),
            IntrinsicId::NarrowFits => Ok(self.narrow_fits(args)),
            IntrinsicId::Unsupported(name) => Err(Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some(format!("unsupported intrinsic `{name}`")),
            })),
            // The concurrency floor is routed by `dispatch_intrinsic` (some of it
            // blocking) and never reaches this value-only dispatcher.
            _ => Err(self.internal("concurrency intrinsic reached value dispatcher")),
        }
    }

    // ---- the concurrency / scheduler floor (v0.11) --------------------

    /// Unwraps a concurrency *receiver* to its scheduler handle. A handle is either
    /// a bare [`Value::Sched`], a one-field handle struct (`Mutex`/`Channel`/
    /// `Atomic`/… wrap their id in field 0), or a pointer to such a struct (a
    /// `*Self` receiver), which is dereferenced through the heap.
    fn handle_of_receiver(&self, recv: &Value) -> Value {
        match recv {
            Value::Sched { .. } => recv.clone(),
            Value::Struct(fields) => fields.first().cloned().unwrap_or(Value::Unit),
            Value::Ptr(p) => match self.heap.load(*p) {
                Ok(inner) => self.handle_of_receiver(&inner),
                Err(_) => Value::Unit,
            },
            _ => recv.clone(),
        }
    }

    /// `@schedSpawn(fn, args_tuple)`: build a fresh fiber whose root frame runs the
    /// `FnRef` callee, seeded from the argument tuple, and return its `Task` handle.
    fn sched_spawn(&mut self, args: &[Value]) -> Result<Value, Halt> {
        // The first operand is the `FnRef` tag; the second (if present) is the
        // argument tuple (a struct/array of by-value arguments).
        let fnid = args.iter().find_map(|a| match a {
            Value::FnRef(f) => Some(*f),
            _ => None,
        });
        let Some(fnid) = fnid else {
            return Err(self.internal("@schedSpawn without a function reference"));
        };
        let call_args: Vec<Value> = match args.iter().find(|a| !matches!(a, Value::FnRef(_))) {
            Some(Value::Struct(fields)) | Some(Value::Array(fields)) => fields.as_ref().clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        let id = self.spawn_fiber_for(fnid, call_args)?;
        Ok(Value::Sched {
            kind: SchedKind::Task,
            id,
        })
    }

    /// `@schedAwait(task)`: if the task is already Done, return its result inline;
    /// otherwise block the current fiber as a joiner of the task.
    fn sched_await(&mut self, args: &[Value]) -> IntrinsicOutcome {
        let Some(tid) = sched_handle(args, SchedKind::Task) else {
            return IntrinsicOutcome::Value(Value::Unit);
        };
        let target = &mut self.sched.fibers[tid as usize];
        if let FiberState::Done = target.state {
            let v = target.result.clone().unwrap_or(Value::Unit);
            return IntrinsicOutcome::Value(unwrap_task_result(v));
        }
        // Register the current fiber as a joiner and park it.
        let cur = self.sched.current;
        target.joiners.push(cur);
        IntrinsicOutcome::Suspend(BlockReason::Join(tid))
    }

    /// `@schedRun()`: drive the ready set to quiescence — the engine behind
    /// `Executor.waitIdle()` and `event.Loop.run()`.
    ///
    /// Each invocation tests whether any *other* fiber is currently runnable
    /// ([`Scheduler::other_runnable`], i.e. present in the FIFO ready queue):
    ///
    /// * If so, it returns [`IntrinsicOutcome::YieldReexec`]. The dispatch site
    ///   rewinds the calling fiber's `pc` to re-point at this very `@schedRun`
    ///   instruction and re-enqueues the fiber at the back of the ready queue. The
    ///   other ready fibers therefore all get a turn first; when the caller cycles
    ///   back to the front it *re-executes* `@schedRun` and re-tests the condition.
    ///   This repeats — driving every transitively-reachable ready fiber (children,
    ///   grandchildren, woken joiners, channel/mutex wakeups) to completion — until
    ///   no other fiber is runnable.
    ///
    /// * Once no other fiber is runnable the drain is quiescent and it returns
    ///   `Value::Unit`, so the caller proceeds.
    ///
    /// Crucially, the loop is bounded by *forward progress*, not by a fixed number
    /// of yields: it stops the instant `other_runnable()` is false. A fiber that is
    /// merely *blocked* (awaiting a handle, parked on a channel/mutex) but has no
    /// ready waker does **not** keep the drain spinning — `other_runnable()` ignores
    /// blocked fibers — so a genuine deadlock falls through to a clean return here
    /// (and, if the caller then has nothing to do either, the event loop's
    /// empty-ready-queue detector reports it; it is never an infinite drain loop).
    fn sched_run(&mut self) -> IntrinsicOutcome {
        if self.sched.other_runnable() {
            IntrinsicOutcome::YieldReexec
        } else {
            IntrinsicOutcome::Value(Value::Unit)
        }
    }

    /// `@chanMake(cap)`: register a channel (cap < 0 is unbounded).
    fn chan_make(&mut self, args: &[Value]) -> Value {
        let cap = args.first().and_then(|v| v.as_i128()).unwrap_or(-1) as i64;
        Value::Sched {
            kind: SchedKind::Channel,
            id: self.sched.make_channel(cap),
        }
    }

    /// `@chanSend(chan, value)`: enqueue (waking a receiver), or block when a
    /// bounded channel is full, or return `false` when the channel is closed.
    fn chan_send(&mut self, args: &[Value]) -> IntrinsicOutcome {
        let Some(cid) = sched_handle(args, SchedKind::Channel) else {
            return IntrinsicOutcome::Value(Value::Bool(false));
        };
        let value = args
            .iter()
            .find(|a| !matches!(a, Value::Sched { .. }))
            .cloned()
            .unwrap_or(Value::Unit);
        let ch = &mut self.sched.channels[cid as usize];
        if ch.closed {
            return IntrinsicOutcome::Value(Value::Bool(false));
        }
        let full = matches!(ch.cap, Some(cap) if ch.queue.len() >= cap);
        if full {
            // Park the sender; the value is admitted when a receiver makes room.
            // The fiber is recorded in `send_waiters` (FIFO) so a receiver can find
            // and admit it; the reason carries the value to enqueue on wake.
            ch.send_waiters.push_back(self.sched.current);
            return IntrinsicOutcome::Suspend(BlockReason::ChanSend { value });
        }
        ch.queue.push_back(value);
        // Wake one parked receiver (it dequeues on resume — see `chan_recv`).
        if let Some(r) = ch.recv_waiters.pop_front() {
            let v = self.sched.channels[cid as usize].queue.pop_front();
            self.sched.wake(r, Some(Value::Optional(v.map(Box::new))));
        }
        IntrinsicOutcome::Value(Value::Bool(true))
    }

    /// `@chanRecv(chan)`: dequeue a value (waking a parked sender, admitting its
    /// value), or return `null` when closed and drained, or block when empty.
    fn chan_recv(&mut self, args: &[Value]) -> IntrinsicOutcome {
        let Some(cid) = sched_handle(args, SchedKind::Channel) else {
            return IntrinsicOutcome::Value(Value::Optional(None));
        };
        let ch = &mut self.sched.channels[cid as usize];
        if let Some(v) = ch.queue.pop_front() {
            // A buffered value: hand it over, then admit one parked sender's value
            // into the freed slot (waking that sender).
            self.admit_parked_sender(cid);
            return IntrinsicOutcome::Value(Value::Optional(Some(Box::new(v))));
        }
        if ch.closed {
            // Drained and closed: the canonical loop terminator.
            return IntrinsicOutcome::Value(Value::Optional(None));
        }
        // A parked sender (only possible on a zero-capacity channel) can hand its
        // value directly to this receiver.
        if let Some(s) = ch.send_waiters.front().copied() {
            if let FiberState::Blocked(BlockReason::ChanSend { value, .. }) =
                &self.sched.fibers[s as usize].state
            {
                let value = value.clone();
                self.sched.channels[cid as usize].send_waiters.pop_front();
                self.sched.wake(s, Some(Value::Bool(true)));
                return IntrinsicOutcome::Value(Value::Optional(Some(Box::new(value))));
            }
        }
        // Empty and open: park the receiver.
        self.sched.channels[cid as usize]
            .recv_waiters
            .push_back(self.sched.current);
        IntrinsicOutcome::Suspend(BlockReason::ChanRecv(cid))
    }

    /// After a `recv` freed a slot, admit one parked sender's value into the queue
    /// and wake it (its send succeeded).
    fn admit_parked_sender(&mut self, cid: u32) {
        let s = self.sched.channels[cid as usize].send_waiters.pop_front();
        if let Some(s) = s {
            if let FiberState::Blocked(BlockReason::ChanSend { value, .. }) =
                &self.sched.fibers[s as usize].state
            {
                let value = value.clone();
                self.sched.channels[cid as usize].queue.push_back(value);
            }
            self.sched.wake(s, Some(Value::Bool(true)));
        }
    }

    /// `@chanClose(chan)`: close and wake all waiters.
    fn chan_close(&mut self, args: &[Value]) {
        if let Some(cid) = sched_handle(args, SchedKind::Channel) {
            self.sched.close_channel(cid);
        }
    }

    /// `@chanLen(chan)`: the buffered count.
    fn chan_len(&self, args: &[Value]) -> Value {
        let n = sched_handle(args, SchedKind::Channel)
            .map(|cid| self.sched.channel_len(cid))
            .unwrap_or(0);
        Value::int(n as i128, IntRepr::USIZE)
    }

    /// `@mutexLock(m)`: acquire if free, else park as a FIFO waiter.
    fn mutex_lock(&mut self, args: &[Value]) -> IntrinsicOutcome {
        let Some(mid) = sched_handle(args, SchedKind::Mutex) else {
            return IntrinsicOutcome::Value(Value::Unit);
        };
        let cur = self.sched.current;
        let m = &mut self.sched.mutexes[mid as usize];
        match m.held_by {
            None => {
                m.held_by = Some(cur);
                IntrinsicOutcome::Value(Value::Unit)
            }
            Some(_) => {
                m.waiters.push_back(cur);
                IntrinsicOutcome::Suspend(BlockReason::MutexLock(mid))
            }
        }
    }

    /// `@mutexUnlock(m)`: release, handing the lock to the first FIFO waiter (so it
    /// wakes already-holding the lock — no thundering re-contention).
    fn mutex_unlock(&mut self, args: &[Value]) {
        let Some(mid) = sched_handle(args, SchedKind::Mutex) else {
            return;
        };
        let next = self.sched.mutexes[mid as usize].waiters.pop_front();
        match next {
            Some(w) => {
                self.sched.mutexes[mid as usize].held_by = Some(w);
                self.sched.wake(w, Some(Value::Unit));
            }
            None => self.sched.mutexes[mid as usize].held_by = None,
        }
    }

    /// `@atomicMake(init)`: register an atomic cell.
    fn atomic_make(&mut self, args: &[Value]) -> Value {
        let init = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        Value::Sched {
            kind: SchedKind::Atomic,
            id: self.sched.make_atomic(init),
        }
    }

    /// `@atomicLoad(a)`: read the cell (as a full-precision integer; the std
    /// wrapper casts to the declared `T`).
    fn atomic_load(&self, args: &[Value]) -> Value {
        let v = sched_handle(args, SchedKind::Atomic)
            .and_then(|aid| self.sched.atomics.get(aid as usize).copied())
            .unwrap_or(0);
        Value::int(v, IntRepr::COMPTIME)
    }

    /// `@atomicStore(a, v)`: write the cell.
    fn atomic_store(&mut self, args: &[Value]) {
        if let Some(aid) = sched_handle(args, SchedKind::Atomic) {
            let v = atomic_operand(args, 0);
            if let Some(cell) = self.sched.atomics.get_mut(aid as usize) {
                *cell = v;
            }
        }
    }

    /// `@atomicFetchAdd(a, delta)`: add and return the previous value.
    fn atomic_fetch_add(&mut self, args: &[Value]) -> Value {
        let mut prev = 0;
        if let Some(aid) = sched_handle(args, SchedKind::Atomic) {
            let delta = atomic_operand(args, 0);
            if let Some(cell) = self.sched.atomics.get_mut(aid as usize) {
                prev = *cell;
                *cell = cell.wrapping_add(delta);
            }
        }
        Value::int(prev, IntRepr::COMPTIME)
    }

    /// `@atomicSwap(a, v)`: store and return the previous value.
    fn atomic_swap(&mut self, args: &[Value]) -> Value {
        let mut prev = 0;
        if let Some(aid) = sched_handle(args, SchedKind::Atomic) {
            let v = atomic_operand(args, 0);
            if let Some(cell) = self.sched.atomics.get_mut(aid as usize) {
                prev = *cell;
                *cell = v;
            }
        }
        Value::int(prev, IntRepr::COMPTIME)
    }

    /// `@atomicCas(a, expected, new)`: compare-and-swap. Returns `null` on success,
    /// else `Some(actual)` — the lock-free retry idiom (spec §4.3).
    fn atomic_cas(&mut self, args: &[Value]) -> Value {
        let Some(aid) = sched_handle(args, SchedKind::Atomic) else {
            return Value::Optional(None);
        };
        let expected = atomic_operand(args, 0);
        let new = atomic_operand(args, 1);
        let cell = match self.sched.atomics.get_mut(aid as usize) {
            Some(c) => c,
            None => return Value::Optional(None),
        };
        if *cell == expected {
            *cell = new;
            Value::Optional(None)
        } else {
            Value::Optional(Some(Box::new(Value::int(*cell, IntRepr::COMPTIME))))
        }
    }

    /// `@wgAdd(wg, n)`: bump the counter.
    fn wg_add(&mut self, args: &[Value]) {
        if let Some(wid) = sched_handle(args, SchedKind::WaitGroup) {
            let n = args
                .iter()
                .filter(|a| !matches!(a, Value::Sched { .. }))
                .find_map(|a| a.as_i128())
                .unwrap_or(0);
            if let Some(wg) = self.sched.waitgroups.get_mut(wid as usize) {
                wg.count += n as i64;
            }
        }
    }

    /// `@wgDone(wg)`: decrement; when the counter reaches zero, wake all waiters.
    fn wg_done(&mut self, args: &[Value]) {
        let Some(wid) = sched_handle(args, SchedKind::WaitGroup) else {
            return;
        };
        let reached_zero = {
            let wg = &mut self.sched.waitgroups[wid as usize];
            wg.count -= 1;
            wg.count <= 0
        };
        if reached_zero {
            let waiters: Vec<FiberId> = self.sched.waitgroups[wid as usize]
                .waiters
                .drain(..)
                .collect();
            for w in waiters {
                self.sched.wake(w, Some(Value::Unit));
            }
        }
    }

    /// `@wgWait(wg)`: return immediately if the counter is already zero, else park.
    fn wg_wait(&mut self, args: &[Value]) -> IntrinsicOutcome {
        let Some(wid) = sched_handle(args, SchedKind::WaitGroup) else {
            return IntrinsicOutcome::Value(Value::Unit);
        };
        if self.sched.waitgroups[wid as usize].count <= 0 {
            return IntrinsicOutcome::Value(Value::Unit);
        }
        let cur = self.sched.current;
        self.sched.waitgroups[wid as usize].waiters.push_back(cur);
        IntrinsicOutcome::Suspend(BlockReason::WaitGroup(wid))
    }

    // ---- the handle-based allocator registry --------------------------

    /// Registers a fresh allocator instance of `kind` (the numeric kind tag the
    /// std `@allocId` passes: 0=Default 1=GPA 2=Arena 3=FixedBuffer 5=Testing),
    /// returning its handle id. `buf` is the caller's `[]u8` for a fixed-buffer
    /// allocator (ignored otherwise).
    fn register_allocator(&mut self, kind: usize, buf: Option<Value>) -> u32 {
        let kind = match kind {
            1 | 5 => AllocatorKind::Gpa, // GPA and testing share the tracker
            2 => AllocatorKind::Arena,
            3 => {
                // The backing buffer is a `[]u8` view; carve from its cell. The
                // caller may pass either a slice (`[]u8`) or a pointer-to-array
                // (`&buf: *[N]u8`, the common `FixedBufferAllocator.init(&storage)`
                // spelling), whose backing cell length is the capacity.
                let (ptr, cap) = match buf {
                    Some(Value::Slice { ptr, len }) => (ptr, len),
                    Some(Value::Ptr(p)) => {
                        let cap = self.heap.len_of(p).unwrap_or(0);
                        (p, cap)
                    }
                    _ => (Ptr::NULL, 0),
                };
                AllocatorKind::FixedBuffer { buf: ptr, cap }
            }
            _ => AllocatorKind::Default,
        };
        let id = self.allocators.len() as u32;
        self.allocators.push(AllocatorState::new(kind));
        id
    }

    /// Allocates an `n`-element run through allocator `id`, returning `Ok([]T)`
    /// (or a clean OutOfMemory panic). Records the cell into the kind's tracker.
    fn alloc_many(&mut self, id: u32, ty: TypeId, n: usize) -> Result<Value, Halt> {
        // A fixed-buffer allocator carves a sub-view of its backing buffer.
        if let Some(AllocatorState {
            kind: AllocatorKind::FixedBuffer { buf, cap },
            offset,
            ..
        }) = self.allocators.get(id as usize)
        {
            let (buf, cap, offset) = (*buf, *cap, *offset);
            if offset + n > cap {
                // Exhausted: a real `error.OutOfMemory` value the caller can
                // `try`/`catch`, matching the FBA contract.
                return Ok(self.out_of_memory_value());
            }
            let view = Value::Slice {
                ptr: Ptr {
                    cell: buf.cell,
                    offset: buf.offset + offset,
                },
                len: n,
            };
            if let Some(st) = self.allocators.get_mut(id as usize) {
                st.offset += n;
            }
            return Ok(Value::ErrOk(Box::new(view)));
        }

        let init = default_value(&self.prog.arena, ty);
        // `alloc_many` validates `n` against a cap and reserves fallibly, so an
        // impossible request becomes a clean out-of-memory PANIC rather than an
        // uncatchable Rust `handle_alloc_error` abort. (A heap-backed allocator
        // surfaces an over-large request as the documented clean panic; only a
        // *bounded* allocator like the fixed-buffer one above hands back a
        // catchable `error.OutOfMemory` for its routine exhaustion.)
        match self.heap.alloc_many(init, n) {
            Ok(ptr) => {
                self.track_alloc(id, ptr.cell);
                Ok(Value::ErrOk(Box::new(Value::Slice { ptr, len: n })))
            }
            Err(f) => Err(self.heap_panic(f)),
        }
    }

    /// Allocates a single `T` through allocator `id`, returning `Ok(*T)`.
    fn alloc_create(&mut self, id: u32, ty: TypeId) -> Result<Value, Halt> {
        let init = default_value(&self.prog.arena, ty);
        let ptr = self.heap.alloc_one(init);
        self.track_alloc(id, ptr.cell);
        Ok(Value::ErrOk(Box::new(Value::Ptr(ptr))))
    }

    /// Reallocates the slice operand through allocator `id` to a new length,
    /// returning `Ok([]T)`. The element type is recovered from the live slice's
    /// own contents (the heap is value-typed), so no type carrier is needed.
    fn alloc_realloc(&mut self, id: u32, args: &[Value]) -> Result<Value, Halt> {
        // Operands: [maybe id], slice, new_len. Find the slice and the length.
        let slice = args.iter().find_map(|a| match a {
            Value::Slice { ptr, len } => Some((*ptr, *len)),
            _ => None,
        });
        let n = args
            .iter()
            .filter(|a| !matches!(a, Value::Slice { .. } | Value::Undef(_)))
            .filter_map(|a| a.as_usize())
            .next_back()
            .unwrap_or(0);
        let (ptr, _len) = slice.unwrap_or((Ptr::NULL, 0));
        // Preserve the element layout: copy through the heap's realloc, which
        // keeps the existing values and frees the old cell.
        let init = Value::int(0, IntRepr::USIZE);
        match self.heap.realloc(ptr, n, init) {
            Ok(new_ptr) => {
                // The old cell is no longer live under this allocator; the new
                // one is. Keep the GPA/arena tracker consistent.
                self.retrack_realloc(id, ptr.cell, new_ptr.cell);
                Ok(Value::ErrOk(Box::new(Value::Slice {
                    ptr: new_ptr,
                    len: n,
                })))
            }
            Err(f) => Err(self.heap_panic(f)),
        }
    }

    /// Frees the slice/pointer operand through allocator `id`. For a GPA/testing
    /// allocator this is the checked path: a double-free or a free of a cell this
    /// allocator never handed out is a clean panic; otherwise the cell moves from
    /// the live set to the freed set and the heap cell is released (so a later
    /// access trips use-after-free). For an arena, `free` is a no-op (the arena
    /// frees in bulk at `deinit`). For a fixed-buffer allocator, `free` is a
    /// no-op (it resets only).
    fn alloc_free(&mut self, id: u32, operand: Option<&Value>) -> Result<Value, Halt> {
        let ptr = match operand {
            Some(Value::Slice { ptr, .. }) => *ptr,
            Some(Value::Ptr(p)) => *p,
            _ => return Ok(Value::Unit),
        };
        match self.allocators.get(id as usize).map(|s| &s.kind) {
            // Arena / fixed-buffer: `free` is a no-op by contract.
            Some(AllocatorKind::Arena | AllocatorKind::FixedBuffer { .. }) => {
                return Ok(Value::Unit)
            }
            Some(AllocatorKind::Gpa) if !self.checks_off => {
                // Checked free. The empty slice (null ptr) is a benign no-op.
                if ptr.is_null() {
                    return Ok(Value::Unit);
                }
                let st = &self.allocators[id as usize];
                if st.freed.contains(&ptr.cell) {
                    return Err(self.clean_panic("double free detected"));
                }
                if !st.live.contains(&ptr.cell) {
                    return Err(self
                        .clean_panic("invalid free: pointer was not allocated by this allocator"));
                }
                let st = &mut self.allocators[id as usize];
                st.live.retain(|&c| c != ptr.cell);
                st.freed.push(ptr.cell);
                self.heap.free(ptr);
                return Ok(Value::Unit);
            }
            _ => {}
        }
        // Default (or checks-off): plain heap free.
        self.heap.free(ptr);
        Ok(Value::Unit)
    }

    /// Frees every cell an arena handed out, all at once (the arena `deinit`
    /// contract), then clears the arena's bookkeeping.
    ///
    /// KNOWN GAP (v0.10 NIT, intentionally not closed): a *forgotten*
    /// `arena.deinit()` is not reported as a leak. Arena allocations are tracked
    /// only under the arena's own handle, never under the backing GPA, and there
    /// is no program-end live-arena report — so dropping an arena without
    /// `deinit` silently reclaims at process exit rather than failing the GPA's
    /// `deinit()` check. Correct code (arena + `deinit`, or arena + `defer
    /// arena.deinit()`) is unaffected; only the misuse pattern goes undetected.
    /// Closing it (routing arena backing through the GPA tracker, or reporting
    /// live arena handles at program end) was deferred to avoid risking false
    /// positives on legitimately-escaping arenas.
    fn arena_deinit(&mut self, id: u32) {
        let cells: Vec<u32> = match self.allocators.get(id as usize) {
            Some(st) => st.live.clone(),
            None => return,
        };
        for cell in cells {
            self.heap.free(Ptr { cell, offset: 0 });
        }
        if let Some(st) = self.allocators.get_mut(id as usize) {
            st.live.clear();
        }
    }

    /// Reports whether allocator `id` (a GPA / testing allocator) leaked — i.e.
    /// any cell it handed out is still live — then clears its tracker. With checks
    /// off (ReleaseFast) this is always `false`, mirroring the stripped safety.
    fn gpa_deinit(&mut self, id: u32) -> bool {
        if self.checks_off {
            return false;
        }
        let leaked = self
            .allocators
            .get(id as usize)
            .map(|st| !st.live.is_empty())
            .unwrap_or(false);
        if let Some(st) = self.allocators.get_mut(id as usize) {
            st.live.clear();
            st.freed.clear();
        }
        leaked
    }

    /// Records a freshly-allocated `cell` into allocator `id`'s tracker (the GPA
    /// leak set, or the arena's bulk-free set). The default allocator and the
    /// checks-off path keep no bookkeeping.
    fn track_alloc(&mut self, id: u32, cell: u32) {
        if self.checks_off {
            return;
        }
        if let Some(st) = self.allocators.get_mut(id as usize) {
            if matches!(st.kind, AllocatorKind::Gpa | AllocatorKind::Arena) {
                st.live.push(cell);
            }
        }
    }

    /// Updates a GPA/arena tracker after a `realloc` moved a live cell to a new
    /// one (the old cell is freed by the heap, the new cell takes its place).
    fn retrack_realloc(&mut self, id: u32, old_cell: u32, new_cell: u32) {
        if self.checks_off {
            return;
        }
        if let Some(st) = self.allocators.get_mut(id as usize) {
            if matches!(st.kind, AllocatorKind::Gpa | AllocatorKind::Arena) {
                st.live.retain(|&c| c != old_cell);
                st.live.push(new_cell);
            }
        }
    }

    /// The `error.OutOfMemory` value an allocator returns when a request cannot be
    /// satisfied (a real error the caller may `try`/`catch`).
    fn out_of_memory_value(&self) -> Value {
        let tag = self
            .prog
            .err_names
            .iter()
            .find(|(_, name)| name.as_str() == "OutOfMemory")
            .map(|(t, _)| t.0)
            .unwrap_or(0);
        Value::ErrVal(tag)
    }

    /// A clean program panic carrying `detail` (used by the GPA double-/foreign-
    /// free traps).
    fn clean_panic(&self, detail: &str) -> Halt {
        Halt::Panic(PanicInfo {
            reason: TrapReason::Panic,
            detail: Some(detail.to_string()),
        })
    }

    // ---- the *System random capability --------------------------------

    /// Advances the splitmix64 PRNG and returns the next 64-bit draw.
    fn next_random(&mut self) -> u64 {
        // splitmix64: a small, well-distributed, dependency-free generator.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fills the `[]u8` operand with PRNG bytes.
    fn random_bytes(&mut self, operand: Option<&Value>) {
        // The buffer may be a `[]u8` slice or a `*[N]u8` pointer-to-array (the
        // `sys.random.bytes(&buf)` spelling), whose backing cell length is the
        // count to fill.
        let (ptr, len) = match operand {
            Some(Value::Slice { ptr, len }) => (*ptr, *len),
            Some(Value::Ptr(p)) => (*p, self.heap.len_of(*p).unwrap_or(0)),
            _ => return,
        };
        for i in 0..len {
            let byte = (self.next_random() & 0xFF) as i128;
            let _ = self.heap.store_index(
                ptr,
                i,
                Value::int(
                    byte,
                    IntRepr {
                        width: 8,
                        signed: false,
                    },
                ),
            );
        }
    }

    /// Implements `@bufPrint(buf, fmt, args)`: format into a scratch buffer, copy
    /// into the caller's `[]u8` cell, and return `Ok([]u8)` of the written prefix
    /// (or `error.NoSpaceLeft` if the formatted text does not fit).
    fn intrinsic_buf_print(&mut self, args: &[Value]) -> Result<Value, Halt> {
        let (buf_ptr, buf_len) = match args.first() {
            Some(Value::Slice { ptr, len }) => (*ptr, *len),
            _ => (Ptr::NULL, 0),
        };
        let fmt_bytes = match args.get(1) {
            Some(Value::Str(b)) => b.clone(),
            _ => Rc::new(Vec::new()),
        };
        let mut arg_values: Vec<Value> = match args.get(2) {
            Some(Value::Struct(fields)) | Some(Value::Array(fields)) => fields.as_ref().clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        self.materialize_str_slice_args(&mut arg_values);
        let mut scratch = Vec::new();
        format_into(&mut scratch, &fmt_bytes, &arg_values).map_err(|e| {
            Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some(e),
            })
        })?;
        if scratch.len() > buf_len {
            // `error.NoSpaceLeft` (fall back to OutOfMemory's slot if absent).
            let tag = self
                .prog
                .err_names
                .iter()
                .find(|(_, name)| name.as_str() == "NoSpaceLeft")
                .map(|(t, _)| t.0)
                .unwrap_or_else(|| match self.out_of_memory_value() {
                    Value::ErrVal(t) => t,
                    _ => 0,
                });
            return Ok(Value::ErrVal(tag));
        }
        for (i, &b) in scratch.iter().enumerate() {
            let _ = self.heap.store_index(
                buf_ptr,
                i,
                Value::int(
                    b as i128,
                    IntRepr {
                        width: 8,
                        signed: false,
                    },
                ),
            );
        }
        Ok(Value::ErrOk(Box::new(Value::Slice {
            ptr: buf_ptr,
            len: scratch.len(),
        })))
    }

    /// The error name for an error-valued operand.
    fn error_name_of(&self, v: &Value) -> String {
        let tag = match v {
            Value::ErrVal(t) => *t,
            Value::Enum { tag, .. } => *tag as u16,
            Value::Int { v, .. } => *v as u16,
            _ => 0,
        };
        self.prog
            .err_names
            .get(&k2_mir::ErrTag(tag))
            .cloned()
            .unwrap_or_else(|| format!("error{tag}"))
    }

    /// Implements `writer.print(fmt, args)`.
    fn intrinsic_print(&mut self, recv: Option<Value>, args: &[Value]) -> Result<Value, Halt> {
        let fmt_bytes = match args.first() {
            Some(Value::Str(b)) => b.clone(),
            _ => Rc::new(Vec::new()),
        };
        // The argument tuple is the second operand (a struct/array of values).
        let mut arg_values: Vec<Value> = match args.get(1) {
            Some(Value::Struct(fields)) | Some(Value::Array(fields)) => fields.as_ref().clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        self.materialize_str_slice_args(&mut arg_values);
        let mut buf = Vec::new();
        format_into(&mut buf, &fmt_bytes, &arg_values).map_err(|e| {
            Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some(e),
            })
        })?;
        match recv {
            Some(Value::Cap(Capability::StderrWriter)) => self.stderr.extend_from_slice(&buf),
            _ => self.stdout.extend_from_slice(&buf),
        }
        Ok(Value::ErrOk(Box::new(Value::Unit)))
    }

    /// The integer representation a check predicate guards: the `undef` carrier's
    /// type when present (it names the result/target type), else the first sized
    /// integer operand's own repr.
    fn check_repr(&self, args: &[Value]) -> IntRepr {
        if let Some(ty) = type_carrier(args) {
            let r = int_repr_of(&self.prog.arena, ty);
            if r.width != 0 {
                return r;
            }
        }
        overflow_repr(args)
    }

    /// The overflow predicate for add/sub/mul: `true` if the operation fits the
    /// carrier type's width.
    fn no_overflow(&self, args: &[Value], kind: ArithKind) -> Value {
        let a = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_i128()).unwrap_or(0);
        let repr = self.check_repr(args);
        let full = match kind {
            ArithKind::Add => a.checked_add(b),
            ArithKind::Sub => a.checked_sub(b),
            ArithKind::Mul => a.checked_mul(b),
        };
        let ok = match full {
            Some(r) => r >= repr.min_value() && r <= repr.max_value(),
            None => false,
        };
        Value::Bool(ok)
    }

    /// The division-overflow predicate for signed `/` and `%`: `true` unless the
    /// dividend is the type's `MIN` and the divisor is `-1` (the one case whose
    /// mathematical result does not fit the type).
    fn no_div_overflow(&self, args: &[Value]) -> Value {
        let a = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_i128()).unwrap_or(0);
        let repr = self.check_repr(args);
        let ok = !(repr.signed && a == repr.min_value() && b == -1);
        Value::Bool(ok)
    }

    /// The negation-overflow predicate: `true` unless negating a signed `MIN`.
    fn no_neg_overflow(&self, args: &[Value]) -> Value {
        let a = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        let repr = self.check_repr(args);
        let ok = !(repr.signed && a == repr.min_value());
        Value::Bool(ok)
    }

    /// The narrowing predicate (`@intCast`): `true` if the value fits the narrower
    /// target type.
    ///
    /// The target type is the `undef` carrier minted from the `@intCast` result
    /// type. When that carrier names a concrete sized integer (the usual case) we
    /// range-check against its true bounds — so a genuinely out-of-range narrow
    /// still traps. But when the carrier is *unsized/unknown* (a `comptime_int` or a
    /// `Deferred` result type — e.g. a monomorphized generic whose `@as(T, …)`
    /// target never concretized, as in `std.atomic.Value(u64).load`), the target
    /// range is genuinely unknown. In that case we must NOT fabricate a signed-i64
    /// range and trap a perfectly valid value (the historical bug: a `u64` above
    /// `i64::MAX` round-tripped through such a cast was rejected): with an unknown
    /// target the narrow is unconstrained — the paired `IntNarrow` is an unmasked
    /// identity that preserves the exact value — so the predicate passes. A
    /// genuinely lossy narrow only happens against a *known* narrower type, which is
    /// still checked precisely.
    fn narrow_fits(&self, args: &[Value]) -> Value {
        let a = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        // Only the explicit carrier names the *target* type; the value operand's own
        // repr describes the SOURCE, never the narrow target, so it must not drive
        // this check (using it would invent an i64 range — see the doc above).
        let target = type_carrier(args).map(|ty| int_repr_of(&self.prog.arena, ty));
        match target {
            // A concrete sized integer target: range-check precisely.
            Some(repr) if repr.width != 0 => {
                Value::Bool(a >= repr.min_value() && a <= repr.max_value())
            }
            // No carrier, or an unsized/unknown (comptime/deferred) target: the
            // narrow target range is unknown, so the cast is unconstrained — pass.
            _ => Value::Bool(true),
        }
    }

    // ---- panic construction -------------------------------------------

    /// A bounds panic with an `i >= len` detail.
    fn bounds_panic(&self, i: usize, len: usize) -> Halt {
        Halt::Panic(PanicInfo {
            reason: TrapReason::Bounds,
            detail: Some(format!("{i} >= {len}")),
        })
    }

    /// Maps a heap fault to a clean panic.
    fn heap_panic(&self, fault: HeapFault) -> Halt {
        let detail = match fault {
            HeapFault::UseAfterFree => "use after free",
            HeapFault::NullPointer => "null pointer dereference",
            HeapFault::OutOfRange => "pointer out of range",
            HeapFault::OutOfMemory => "out of memory",
        };
        Halt::Panic(PanicInfo {
            reason: TrapReason::Panic,
            detail: Some(detail.to_string()),
        })
    }

    /// An internal VM fault that should be impossible after `verify`; surfaced as
    /// a clean panic rather than a Rust panic.
    fn internal(&self, what: &str) -> Halt {
        Halt::Panic(PanicInfo {
            reason: TrapReason::Panic,
            detail: Some(format!("internal VM error: {what}")),
        })
    }
}

/// Control-flow outcome of a single instruction.
enum Flow {
    /// Advance to the next instruction.
    Next,
    /// The pc was already set (a jump/branch/switch/call).
    Jumped,
    /// The current frame returns this value.
    Returned(Value),
    /// A blocking intrinsic parked the current fiber; stop running it.
    Suspend,
    /// An explicit cooperative yield; the fiber stays Ready and is re-enqueued. This
    /// covers both `@schedYield` (the fiber resumes at the statement *after* the
    /// yield) and the drive-to-quiescence `@schedRun` (whose dispatch arm first
    /// rewinds the `pc` so it *re-executes* on resume — see the `YieldReexec` arm in
    /// [`Vm::step`] and [`Vm::sched_run`]). Either way the event loop re-enqueues the
    /// fiber, so the loop layer treats both identically.
    Yield,
}

/// How a fiber stopped running (returned by [`Vm::run_fiber`]).
enum Suspend {
    /// Parked on a wait reason (a waker will re-ready it later).
    Blocked,
    /// Explicitly yielded; still Ready, to be re-enqueued by the event loop.
    Yielded,
    /// The fiber's root frame returned this value (the fiber completes).
    Completed(Value),
}

/// The result of dispatching an intrinsic: either a value to write into the dst
/// register, or a scheduler signal (suspend the fiber, or yield it).
enum IntrinsicOutcome {
    /// A plain result value (the non-blocking case).
    Value(Value),
    /// Park the current fiber on this reason; resume into the intrinsic's dst.
    Suspend(BlockReason),
    /// Cooperatively yield the current fiber (it stays Ready) — `@schedYield`. The
    /// fiber resumes at the statement *after* the yield.
    Yield,
    /// Yield the current fiber for *re-execution* of the same intrinsic — `@schedRun`.
    /// The fiber stays Ready, but its `pc` is rewound so the intrinsic runs again on
    /// resume. This drives the drain loop to quiescence (see [`Vm::sched_run`]).
    YieldReexec,
}

/// The arithmetic op an overflow predicate guards.
enum ArithKind {
    Add,
    Sub,
    Mul,
}

/// The integer representation a check predicate guards. The predicate's `undef`
/// carrier names the *result* type, but each live integer operand already knows
/// its own width/sign (carried on the [`Value::Int`]), so the simplest robust
/// source is the first real integer operand — exactly the value the check
/// guards. Falls back to a 64-bit signed repr if no sized operand is present.
fn overflow_repr(args: &[Value]) -> IntRepr {
    for a in args {
        if let Value::Int { repr, .. } = a {
            if repr.width != 0 {
                return *repr;
            }
        }
    }
    IntRepr {
        width: 64,
        signed: true,
    }
}

/// Reads the element `TypeId` carried by a `create`/`alloc` intrinsic's `undef`
/// argument.
fn type_carrier(args: &[Value]) -> Option<TypeId> {
    args.iter().find_map(|a| match a {
        Value::Undef(ty) => Some(*ty),
        _ => None,
    })
}

/// The declared kind of a build option, derived from its concrete type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OptionKind {
    /// A `bool` option (`-Dflag=true|false|1|0|yes|no|on|off`).
    Bool,
    /// An integer option (any signed/unsigned width or `comptime_int`).
    Int,
    /// A `[]const u8` (string) option, the default for any other type.
    String,
}

/// Maps a build option's declared type (carried as the `undef` TYPE of the first
/// `@buildOption` operand) to its [`OptionKind`]. The MIR lowerer gives a
/// type-denoting argument its DENOTED type, so `bool` → `Bool`, any integer width
/// → `Int`, and anything else (`[]const u8`, a named type) → `String`. A still-
/// erased carrier (`type`/deferred) falls back to `String`, the safe default that
/// never crashes a build.
fn option_kind_of(carrier: Option<TypeId>, arena: &k2_types::TypeArena) -> OptionKind {
    if let Some(t) = carrier {
        match arena.get(t) {
            k2_types::Type::Bool => return OptionKind::Bool,
            k2_types::Type::Int { .. } | k2_types::Type::ComptimeInt => return OptionKind::Int,
            _ => {}
        }
    }
    OptionKind::String
}

/// `true` if `id` is one of the concurrency / scheduler intrinsics (routed by
/// [`Vm::dispatch_concurrency`], some of it blocking).
fn is_concurrency_intrinsic(id: &IntrinsicId) -> bool {
    matches!(
        id,
        IntrinsicId::SchedSpawn
            | IntrinsicId::SchedYield
            | IntrinsicId::SchedAwait
            | IntrinsicId::SchedRun
            | IntrinsicId::ChanMake
            | IntrinsicId::ChanSend
            | IntrinsicId::ChanRecv
            | IntrinsicId::ChanClose
            | IntrinsicId::ChanLen
            | IntrinsicId::MutexMake
            | IntrinsicId::MutexLock
            | IntrinsicId::MutexUnlock
            | IntrinsicId::AtomicMake
            | IntrinsicId::AtomicLoad
            | IntrinsicId::AtomicStore
            | IntrinsicId::AtomicFetchAdd
            | IntrinsicId::AtomicSwap
            | IntrinsicId::AtomicCas
            | IntrinsicId::WgMake
            | IntrinsicId::WgAdd
            | IntrinsicId::WgDone
            | IntrinsicId::WgWait
    )
}

/// `true` for the `*Build` capability floor — the recording intrinsics behind the
/// bundled `build` module. Routed to [`Vm::dispatch_build`].
fn is_build_intrinsic(id: &IntrinsicId) -> bool {
    matches!(
        id,
        IntrinsicId::BuildStdTarget
            | IntrinsicId::BuildStdOptimize
            | IntrinsicId::BuildOption
            | IntrinsicId::BuildAddLibrary
            | IntrinsicId::BuildAddExecutable
            | IntrinsicId::BuildAddTest
            | IntrinsicId::BuildArtifactOption
            | IntrinsicId::BuildArtifactModule
            | IntrinsicId::BuildArtifactModuleSelf
            | IntrinsicId::BuildArtifactForwardArgs
            | IntrinsicId::BuildAddRun
            | IntrinsicId::BuildInstall
            | IntrinsicId::BuildStep
            | IntrinsicId::BuildStepDependOn
            | IntrinsicId::BuildPath
            | IntrinsicId::BuildFmt
            | IntrinsicId::BuildTargetArch
            | IntrinsicId::BuildTargetOs
            | IntrinsicId::BuildTargetAbi
            | IntrinsicId::BuildArtifactStep
    )
}

/// Maps a `Target` field-read intrinsic to the struct field index it reads
/// (`arch`=0, `os`=1, `abi`=2), or `None` for any other build intrinsic.
fn build_target_field(id: &IntrinsicId) -> Option<usize> {
    match id {
        IntrinsicId::BuildTargetArch => Some(0),
        IntrinsicId::BuildTargetOs => Some(1),
        IntrinsicId::BuildTargetAbi => Some(2),
        _ => None,
    }
}

/// `true` for the `*Artifact`/`*Step` methods whose handle id lives in the
/// RECEIVER (`{ id }` struct), so the dispatcher prepends it to the operands. The
/// `*Build` methods (`standardTarget`, `addLibrary`, `addRunArtifact`, …) take a
/// bare `*Build` receiver carrying no id, so they are excluded.
fn needs_handle_receiver(id: &IntrinsicId) -> bool {
    matches!(
        id,
        IntrinsicId::BuildArtifactOption
            | IntrinsicId::BuildArtifactModule
            | IntrinsicId::BuildArtifactModuleSelf
            | IntrinsicId::BuildArtifactForwardArgs
            | IntrinsicId::BuildArtifactStep
            | IntrinsicId::BuildStepDependOn
    )
}

/// A short human description of a [`Halt`], used by the test runner to report why
/// a test failed (an error name, a panic message, or an explicit exit).
fn halt_detail(h: &Halt) -> String {
    match h {
        Halt::ProgramError(tag) => format!("error tag {tag}"),
        Halt::Panic(info) => format!("panic: {}", info.message()),
        Halt::Exit(c) => format!("exit {c}"),
    }
}

/// Finds the scheduler-object handle id of `kind` among an intrinsic's operands.
/// (Concurrency intrinsics take the object handle as their receiver/first operand,
/// e.g. `@chanSend(chan, value)` or `@mutexLock(m)`.)
fn sched_handle(args: &[Value], want: SchedKind) -> Option<u32> {
    args.iter().find_map(|a| match a {
        Value::Sched { kind, id } if *kind == want => Some(*id),
        _ => None,
    })
}

/// Reads the `n`-th non-handle integer operand of an atomic op as an `i128` (skips
/// the leading `Sched` handle). Used for `store`/`fetchAdd`/`swap`/`cas` operands.
fn atomic_operand(args: &[Value], n: usize) -> i128 {
    args.iter()
        .filter(|a| !matches!(a, Value::Sched { .. }))
        .filter_map(|a| a.as_i128())
        .nth(n)
        .unwrap_or(0)
}

/// Unwraps a task's result for delivery to an `await`. A fiber's root frame may
/// return an `Ok(v)`/bare value; `await` yields the inner value so a `void` task
/// awaits cleanly and a value task yields its `T`.
fn unwrap_task_result(v: Value) -> Value {
    match v {
        Value::ErrOk(inner) => *inner,
        other => other,
    }
}

/// The handle id carried by an `Allocator` receiver value (the `Capability::
/// Allocator(id)` from `sys.heap` or an `@allocHandle`). A non-allocator receiver
/// (or an absent one) falls back to the default allocator id `0`.
fn alloc_id_of(recv: &Option<Value>) -> u32 {
    match recv {
        Some(Value::Cap(Capability::Allocator(id))) => *id,
        _ => 0,
    }
}

/// The handle id for a `realloc` reached as `alloc.realloc(slice, n)`: the id is
/// on the receiver. When the floor `@reallocRaw(id, slice, n)` form is used, the
/// id is instead the first integer operand; we prefer the receiver if present.
fn alloc_id_for_realloc(recv: &Option<Value>, args: &[Value]) -> u32 {
    if let Some(Value::Cap(Capability::Allocator(id))) = recv {
        return *id;
    }
    // The floor form: the leading `u32` operand before the slice is the id.
    for a in args {
        match a {
            Value::Slice { .. } => break,
            Value::Int { v, .. } if *v >= 0 => return *v as u32,
            _ => {}
        }
    }
    0
}

/// The shift amount, taken modulo the operand width for safety (an over-wide
/// shift is defined to wrap rather than panic the Rust host).
fn shift_amount(y: i128, repr: IntRepr) -> u32 {
    let w = if repr.width == 0 {
        128
    } else {
        repr.width as u32
    };
    ((y.rem_euclid(w as i128)) as u32) % w
}

/// `true` if both operands are `Value::Bool` — the condition under which a
/// bitwise `&`/`|`/`^` is a boolean op (`@reduce(.And/.Or/.Xor)` over a bool
/// vector) and must yield a `bool` result, matching the native backend.
fn both_bool(a: &Value, b: &Value) -> bool {
    matches!(a, Value::Bool(_)) && matches!(b, Value::Bool(_))
}

/// The [`IntRepr`] to compare two operands under. A relational op's instruction
/// `repr` is the *result* type (`bool`, width 0), which is wrong for the
/// comparison itself; the live operands carry their own width/signedness, so we
/// prefer the first sized-integer operand's repr (falling back to the second,
/// then to the instruction repr). This makes `u128`/`usize` operands compare
/// unsigned even though the result type is `bool`.
fn operand_cmp_repr(a: &Value, b: &Value, fallback: IntRepr) -> IntRepr {
    for v in [a, b] {
        if let Value::Int { repr, .. } = v {
            if repr.width != 0 {
                return *repr;
            }
        }
    }
    fallback
}

/// Width/sign-aware integer comparison: both operands are normalized to `repr`,
/// then compared (signed or unsigned per the repr).
fn int_cmp(x: i128, y: i128, repr: IntRepr) -> std::cmp::Ordering {
    if repr.signed || repr.width == 0 {
        x.cmp(&y)
    } else {
        (x as u128).cmp(&(y as u128))
    }
}
