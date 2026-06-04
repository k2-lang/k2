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

use crate::compile::{compile_program, default_value, int_repr_of};
use crate::fmt::format_into;
use crate::heap::{Heap, HeapFault, Ptr};
use crate::isa::{AggregateKind, CompiledFn, Instr, IntrinsicId, Reg, StoreStep};
use crate::value::{Capability, IntRepr, Value};

/// How far the VM may step before it gives up on a presumed-nonterminating
/// program. Real corpus programs finish in a few thousand steps; this is still
/// five orders of magnitude larger so no genuine program is cut off, yet small
/// enough to bound the worst case. A [`WALL_DEADLINE`] runs alongside it so a
/// runaway loop is reported within a few seconds even under the (slower) debug
/// build, where 2e8 steps would otherwise take ~30s.
const STEP_BUDGET: u64 = 200_000_000;

/// The wall-clock deadline: a program that runs longer than this is presumed
/// nonterminating and reported with the same clean budget panic as the step
/// counter. This makes a runaway loop terminate in a few seconds regardless of
/// the per-step cost of the build, while real corpus programs (milliseconds)
/// are unaffected. Checked only every [`WALL_CHECK_INTERVAL`] steps so the
/// `Instant::now()` cost is negligible.
const WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// How often (in steps) the wall-clock deadline is polled.
const WALL_CHECK_INTERVAL: u64 = 1 << 20;

/// The maximum call depth before a clean `stack overflow` panic (rather than a
/// Rust stack overflow — though the VM does not recurse in Rust, this bounds
/// pathological heap-frame growth).
const MAX_DEPTH: usize = 200_000;

/// A reason the VM stopped: a clean program panic, an error that escaped `main`,
/// or an explicit exit.
pub enum Halt {
    /// A safety check failed or a trap/unreachable fired.
    Panic(PanicInfo),
    /// `main` returned an error value (its name is printed by the caller).
    ProgramError(u16),
    /// An explicit process exit with a code.
    Exit(i32),
}

/// The detail of a clean panic.
pub struct PanicInfo {
    /// Why the panic fired.
    pub reason: TrapReason,
    /// An optional extra detail (e.g. the offending index/length).
    pub detail: Option<String>,
}

impl PanicInfo {
    /// The human-readable panic message (without the `panic: ` prefix).
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

/// One call frame: the callee, its register file, and its program counter.
struct Frame {
    /// The compiled function this frame is executing.
    fnid: FnId,
    /// The register file (registers 1:1 with MIR locals, plus scratch).
    regs: Vec<Value>,
    /// The instruction pointer within the function's code.
    pc: usize,
    /// The caller's destination register for this call's result.
    ret_reg: Reg,
}

/// The virtual machine.
pub struct Vm<'p> {
    prog: &'p MirProgram,
    compiled: Vec<CompiledFn>,
    heap: Heap,
    frames: Vec<Frame>,
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
}

impl<'p> Vm<'p> {
    /// Builds a VM over a lowered program, compiling every function up front.
    pub fn new(prog: &'p MirProgram) -> Vm<'p> {
        let release_fast = matches!(prog.mode, k2_mir::BuildMode::ReleaseFast);
        Vm {
            prog,
            compiled: compile_program(prog),
            heap: Heap::new(release_fast),
            frames: Vec::new(),
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
        }
    }

    /// The number of instructions executed so far — the deterministic metric the
    /// benchmark harness reports (Debug vs ReleaseFast).
    pub fn instr_count(&self) -> u64 {
        self.instr_count
    }

    /// Runs `main(sys)` to completion. On success returns `Ok(())`; otherwise a
    /// [`Halt`] describing the panic / program error / exit.
    pub fn run_main(&mut self) -> Result<(), Halt> {
        let main = self.find_main().ok_or(Halt::Exit(0))?;
        // Build the root `*System` capability and call `main(sys)`.
        let sys = Value::Cap(Capability::System);
        let result = self.call_top(main, vec![sys])?;
        // `main` returns `!void`: inspect the result.
        match result {
            Value::ErrVal(tag) => Err(Halt::ProgramError(tag)),
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

    /// Calls `fnid` with `args` from the top level (no caller frame), running the
    /// VM loop until that call's frame returns, and yields the returned value.
    fn call_top(&mut self, fnid: FnId, args: Vec<Value>) -> Result<Value, Halt> {
        self.push_frame(fnid, args, 0)?;
        let base_depth = self.frames.len();
        self.run_until_depth(base_depth - 1)
    }

    /// Pushes a new frame for `fnid`, seeding its parameter registers with `args`.
    fn push_frame(&mut self, fnid: FnId, args: Vec<Value>, ret_reg: Reg) -> Result<(), Halt> {
        if self.frames.len() >= MAX_DEPTH {
            return Err(Halt::Panic(PanicInfo {
                reason: TrapReason::Panic,
                detail: Some("stack overflow".to_string()),
            }));
        }
        let cf = &self.compiled[fnid.index()];
        let mut regs = vec![Value::Unit; cf.num_regs];
        // Params occupy registers 1..=args.len() (register 0 is the return slot).
        for (i, a) in args.into_iter().enumerate() {
            regs[i + 1] = a;
        }
        self.frames.push(Frame {
            fnid,
            regs,
            pc: 0,
            ret_reg,
        });
        // Initialize address-taken parameters into heap homes immediately, so a
        // `&param` / `self.*` receiver works. (Locals get their homes at their
        // StorageLive via InitAddrLocal.)
        self.init_addr_taken_params(fnid);
        Ok(())
    }

    /// Boxes any `address_taken` parameter into a heap cell at frame entry.
    fn init_addr_taken_params(&mut self, fnid: FnId) {
        let cf = &self.compiled[fnid.index()];
        let func = &self.prog.funcs[fnid.index()];
        let nparams = func.params.len();
        // Collect the param register indices that are address-taken.
        let to_box: Vec<usize> = (1..=nparams).filter(|&i| cf.addr_taken[i]).collect();
        for i in to_box {
            let cur = self.frames.last().unwrap().regs[i].clone();
            let ptr = self.heap.alloc_one(cur);
            self.frames.last_mut().unwrap().regs[i] = Value::Ptr(ptr);
        }
    }

    /// Runs the dispatch loop until the frame stack shrinks back to
    /// `target_depth` (i.e. the just-pushed top frame has returned), yielding the
    /// value that frame returned.
    fn run_until_depth(&mut self, target_depth: usize) -> Result<Value, Halt> {
        loop {
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

            let depth = self.frames.len();
            if depth == 0 {
                // Should not happen; the loop exits via the Return handler.
                return Ok(Value::Unit);
            }
            let frame = self.frames.last_mut().unwrap();
            let fnid = frame.fnid;
            let pc = frame.pc;
            let instr = self.compiled[fnid.index()].code[pc].clone();
            frame.pc += 1;

            match self.step(instr)? {
                Flow::Next => {}
                Flow::Jumped => {}
                Flow::Returned(v) => {
                    // Pop the returning frame; deliver its value to the caller.
                    let done = self.frames.pop().unwrap();
                    if self.frames.len() == target_depth {
                        return Ok(v);
                    }
                    // Write the result into the caller's destination register.
                    let caller = self.frames.last_mut().unwrap();
                    Self::set_reg(caller, done.ret_reg, v);
                }
            }
        }
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
            Instr::Jump { target } => {
                self.frames.last_mut().unwrap().pc = target;
                Ok(Flow::Jumped)
            }
            Instr::Branch {
                cond,
                then_pc,
                else_pc,
            } => {
                let c = self.get(cond).as_bool().unwrap_or(false);
                self.frames.last_mut().unwrap().pc = if c { then_pc } else { else_pc };
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
                self.frames.last_mut().unwrap().pc = target;
                Ok(Flow::Jumped)
            }
            Instr::Return { src } => {
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
                let v = self.dispatch_intrinsic(&id, recv_v, &argv)?;
                self.set_cur(dst, v);
                Ok(Flow::Next)
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

    /// The current frame's function id.
    fn cur_fnid(&self) -> FnId {
        self.frames.last().unwrap().fnid
    }

    /// Reads register `r` of the current frame.
    fn get(&self, r: Reg) -> Value {
        self.frames.last().unwrap().regs[r as usize].clone()
    }

    /// Writes register `r` of the current frame.
    fn set_cur(&mut self, r: Reg, v: Value) {
        let frame = self.frames.last_mut().unwrap();
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
            CastKind::PtrReinterpret => a.clone(),
            CastKind::Widen | CastKind::IntNarrow => {
                if to_float {
                    Value::Float(a.as_f64().unwrap_or(0.0))
                } else {
                    Value::int(a.as_i128().unwrap_or(0), to)
                }
            }
        }
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
    fn index_of(&self, base: &Value, index: &Value) -> Result<Value, Halt> {
        let i = index.as_usize().unwrap_or(usize::MAX);
        match base {
            Value::Array(elems) => elems
                .get(i)
                .cloned()
                .ok_or_else(|| self.bounds_panic(i, elems.len())),
            // Indexing a `[]const u8` string yields its `i`-th byte as a `u8`.
            Value::Str(bytes) => bytes
                .get(i)
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
            Value::Slice { ptr, len } => {
                if i >= *len {
                    return Err(self.bounds_panic(i, *len));
                }
                self.heap
                    .load_index(*ptr, i)
                    .map_err(|f| self.heap_panic(f))
            }
            Value::Ptr(p) => {
                // A pointer-to-array used as a slice (e.g. `&arr` passed to a
                // `[]T` parameter): index through the boxed array/allocation.
                match self.heap.load(*p) {
                    Ok(Value::Array(elems)) => elems
                        .get(i)
                        .cloned()
                        .ok_or_else(|| self.bounds_panic(i, elems.len())),
                    Ok(_) => self.heap.load_index(*p, i).map_err(|f| self.heap_panic(f)),
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
                    if i >= n {
                        return Err(self.bounds_panic(i, n));
                    }
                    Ok(default_value(&self.prog.arena, *elem))
                }
                _ => Err(self.internal("index of non-indexable value")),
            },
            _ => Err(self.internal("index of non-indexable value")),
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
                let i = self.get(*reg).as_usize().unwrap_or(usize::MAX);
                match slot {
                    Value::Array(elems) => {
                        let v = Rc::make_mut(elems);
                        let n = v.len();
                        let target = v.get_mut(i).ok_or_else(|| self.bounds_panic(i, n))?;
                        self.store_into(target, rest, value)
                    }
                    Value::Slice { ptr, len } => {
                        if i >= *len {
                            return Err(self.bounds_panic(i, *len));
                        }
                        if rest.is_empty() {
                            self.heap
                                .store_index(*ptr, i, value)
                                .map_err(|f| self.heap_panic(f))
                        } else {
                            let mut inner = self
                                .heap
                                .load_index(*ptr, i)
                                .map_err(|f| self.heap_panic(f))?;
                            self.store_into(&mut inner, rest, value)?;
                            self.heap
                                .store_index(*ptr, i, inner)
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

    /// Dispatches a resolved intrinsic.
    fn dispatch_intrinsic(
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
        }
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
        let arg_values: Vec<Value> = match args.get(2) {
            Some(Value::Struct(fields)) | Some(Value::Array(fields)) => fields.as_ref().clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
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
        let arg_values: Vec<Value> = match args.get(1) {
            Some(Value::Struct(fields)) | Some(Value::Array(fields)) => fields.as_ref().clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
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

    /// The narrowing predicate: `true` if the value fits the narrower target.
    fn narrow_fits(&self, args: &[Value]) -> Value {
        let a = args.first().and_then(|v| v.as_i128()).unwrap_or(0);
        let repr = self.check_repr(args);
        Value::Bool(a >= repr.min_value() && a <= repr.max_value())
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
