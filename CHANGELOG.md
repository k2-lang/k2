# Changelog

All notable changes to k2 are recorded here. The format is loosely based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it
reaches `0.1.0`.

While the version is `0.0.x`, **anything may change at any time** — the language
is being designed in the open and nothing is stable yet.

## [Unreleased]

### Added

- **Language design.** The full specification of k2 — *Kardashev Type II:
  total control over the machine, with zero waste* — a systems language that
  takes Zig's design philosophy (no hidden control flow, no hidden allocation,
  no ambient authority, `comptime` as the only metaprogramming, errors as
  values, native speed with no runtime and no GC) and implements its toolchain
  in Rust.
  - `docs/philosophy.md` — the design pillars and what k2 keeps, drops, and
    changes relative to its inspirations.
  - `docs/spec/01`–`10` — lexical structure, types, expressions and statements,
    functions, memory and allocators, error handling, `comptime`, modules and
    the build system, concurrency, and the standard library.
  - `docs/grammar.ebnf` — the complete reference grammar.
  - `docs/compiler-architecture.md` — the planned pipeline and the dual
    Cranelift (debug) / LLVM (release) backend strategy.
  - `docs/tooling.md` — the `k2c` driver, `build.k2`, and the formatter.
  - `examples/` — runnable `.k2` programs covering hello-world, allocators,
    error handling, `comptime` reflection, generics, and a `build.k2`.

- **Toolchain front-end (Rust).** A Cargo workspace using only the standard
  library, so it builds and tests fully offline.
  - `k2-lexer` — a complete, recovering lexer for the surface syntax, with an
    extensive unit-test suite.
  - `k2-syntax` — the AST type definitions and source-span machinery.
  - `k2-vm` — the v0.8 bytecode compiler + register VM + runtime shim: it
    compiles the monomorphized MIR to a compact register ISA and executes
    `main(sys)` on a managed heap, with the minimal io/heap capability
    intrinsics (`sys.io.stdout`/`stderr`, `Writer.print`, `sys.heap` with
    `create`/`destroy`/`alloc`/`free`). A failed safety check / `Trap` /
    `unreachable` becomes a clean runtime panic (a `panic:` line on stderr and
    a nonzero exit), never an uncontrolled Rust panic; `defer`/`errdefer`
    ordering and `try` error-propagation execute straight from the CFG.
  - `k2c` — the compiler driver, with a working `tokenize` / `lex` subcommand
    that streams tokens from a file or standard input, plus the `run`
    subcommand that compiles and executes a program (Debug or `--release-fast`).
  - `k2-opt` — the v0.9 MIR optimizer: a pass pipeline run to a fixpoint
    (constant folding, constant/copy propagation, dead-code/dead-store
    elimination, CFG simplification, small-monomorphic-call inlining /
    devirtualization with size + recursion budgets, and — in ReleaseSafe — sound
    removal of provably-redundant realized safety checks). The optimizer is
    sound by construction: it only substitutes provably-equal values, deletes
    provably-dead effect-free instructions (demoting an impure dead-result store
    to an `Eval` so its effect and any trap are preserved), rewrites the CFG
    behavior-preservingly, or removes a check whose success edge is provably
    always taken. `MirProgram::verify` holds after every pass. Build modes are
    wired end to end (`run`/`mir --release-safe`/`--release-fast` optimize;
    Debug stays unoptimized unless `--opt`).
  - `k2c bench` — a reproducible benchmark harness that measures *executed VM
    instructions* (deterministic, not wall-clock) under Debug vs ReleaseFast
    over a committed set of bench programs, asserts the optimized output is
    byte-identical to the unoptimized output, and reports the reduction
    (~50% fewer instructions / ~2x across the suite). A differential corpus
    test asserts opt == unopt behavior in every mode (a single divergence is a
    blocker) and Debug == ReleaseSafe strictly.
  - **Concurrency (v0.11).** A deterministic **cooperative fiber scheduler** in
    `k2-vm` (`crate::sched`): each spawned unit of work is a green fiber with its
    own call-frame stack, and a single-threaded event loop interleaves ready
    fibers at explicit yield points (`spawn`/channel `send`-`recv`/`Mutex`
    acquire/`await`/`yield`). A FIFO ready queue plus FIFO waiter lists make the
    interleaving — and thus the output — reproducible run to run; an
    "all-fibers-blocked" state is reported as a clean deadlock diagnostic rather
    than a hang. The std concurrency surface is written in k2 over a small set of
    scheduler `@builtin` leaf intrinsics: `std.Thread.Executor`/`Task` (capability-
    passed spawn + join), `std.Channel(T)` (bounded/unbounded mpsc with blocking
    `send`/`recv` and `close`), `std.Thread.Mutex`/`WaitGroup`, `std.atomic.Value(T)`
    (`load`/`store`/`fetchAdd`/`swap`/`cmpxchg*` with explicit `Ordering`), and the
    colorless, keyword-free `std.event.Loop`/`Future` async surface
    (`loop.spawn(f, args)` + `future.await(loop, T)`). Every object is a value built
    from `sys.heap` and passed explicitly, never a global. OS-thread parallelism
    and the stackless async lowering are the native-backend realization of the
    same API; the VM realizes it via fibers (documented in
    `docs/spec/09-concurrency.md §8.1` and `crate::sched`). New example
    `examples/concurrency.k2` (spawn+join parallel sum, channel producer/consumer,
    mutex counter, atomics, async/await) runs with deterministic output.
  - **The build system is k2 + the package/module system (v0.12).** `build.k2`
    now *runs*: `build(b: *Build)` is ordinary k2 executed on the VM with a
    `*Build` **capability** — the build-time analogue of `*System`. Its methods
    bottom out in a floor of `@build*` **recording** intrinsics (no I/O, no real
    allocation — the comptime sandbox is honored) that build a deterministic,
    creation-ordered **build graph** the VM exposes after `build(b)` returns. The
    bundled `build` module (`crates/k2-std/std/build.k2`) declares the `Build`
    capability surface and its `Target`/`OptimizeMode`/`Step`/`Module`/`Artifact`
    helper types over that floor. A new `k2c build [step] [-Dkey=value …]`
    subcommand runs `build(b)`, parses `-Doptimize`/`-Dtarget`/custom options,
    writes a deterministic, reproducible `build.lock`, then executes the step:
    `install`/default **describes + validates** the DAG (native artifact emission
    is a documented no-op until post-0.13 native codegen), `run` **builds + runs**
    the chosen executable through the VM, and `test` **compiles + runs** the
    reachable `test { ... }` blocks. **Multi-file compilation** is realized by
    merging the module graph into one implicit-struct `SourceFile` (the
    std-injection move, generalized): `.k2` **path imports** (`@import("./x.k2")`)
    and **named modules** (`exe.addModule("name", lib.module())`, then
    `@import("name")` in the artifact) now resolve, type-check, monomorphize,
    lower, and run as one program — wired into `k2c run` as well, with the
    single-file fast path untouched. `@import("build_options")` is a **synthesized
    comptime module** (one `pub const` per `addOption`), so `if (opts.flag)` is a
    comptime-known condition whose dead branch the optimizer eliminates entirely.
    Fixes a latent checker/lowering bug where a **non-generic free function called
    through a namespace const** (`ns.add(x, y)`) was lowered with a spurious
    receiver. New `examples/support/root.k2` + `examples/tests/all.k2` make
    `examples/build.k2` run end to end: `k2c build` describes the DAG,
    `k2c build run -Dexample=hello` prints `Hello, k2!`, and `k2c build test` runs
    the example tests.

- **Native x86-64 backend foundation (v0.14).** A new pure-std crate
  `k2-codegen` that turns a *subset* of the monomorphized MIR into a real,
  static, directly-runnable x86-64 Linux ELF — with **no** Cranelift, **no**
  LLVM, **no** libc, and **no** third-party crates. It has three hand-rolled
  layers: a byte-exact **x86-64 instruction encoder** (REX/ModRM/SIB by hand:
  `mov`/`add`/`sub`/`imul`/`cqo`+`idiv`, `cmp`/`test`, `and`/`or`/`xor`,
  `shl`/`shr`/`sar`, `setcc`/`movzx`/`movsx`/`movsxd`, `lea`, `push`/`pop`,
  near `call`/`jmp`/`jcc` with `rel32` fixups, `syscall`, and the `[rbp - N]`
  stack-slot + immediate addressing modes), an **ELF64 writer** that emits a
  static non-PIE `ET_EXEC` at base `0x400000` (entry `0x401000`, one RX `PT_LOAD`
  for headers+code and an R-only `PT_LOAD` for `.rodata`, no dynamic linker / no
  section headers), and a **MIR → machine-code lowering** that gives each MIR
  local an `[rbp - 8*(i+1)]` stack slot and lowers width-correct integer
  arithmetic / compare / bitwise / shift, `Goto`/`Branch`/`Switch`/`Return`/
  `Trap`/`Unreachable`, System V AMD64 direct calls (args in
  `rdi/rsi/rdx/rcx/r8/r9`, result in `rax`, 16-byte-aligned call sites), the
  `@no_*_overflow`/`narrow_fits` safety predicates that guard a `Trap`, and the
  `write`/`exit` syscall intrinsics (`sys.io.stdout()`/`stderr()` → an fd token;
  a fixed-string `print` → a `write(fd, ptr, len)` of `.rodata` bytes; a `Trap`
  → `panic: …` on stderr + `exit(134)`, matching the VM). A `_start` shim runs
  `main` and `exit()`s with its result. Two new driver subcommands wire it in:
  **`k2c run-native <file.k2>`** compiles to a temp ELF, executes it, and
  propagates the exit code, and **`k2c build-native <file.k2> -o <out>`** writes
  the `chmod +x`-able ELF. Anything outside the subset (floats, aggregates,
  projected places, runtime-formatted `print`, …) is rejected up-front with a
  clean `error: native backend: …` message that points back to `k2c run` — it is
  never miscompiled, and all existing subcommands are untouched. The encoder
  asserts exact bytes against `as`/`objdump`-verified encodings and the ELF
  writer validates its header / segment invariants on **every** host; the
  native-execution tests (which actually **run** the emitted binary and assert
  exit code + stdout, **differentially against `k2c run`**) are gated to
  `x86_64`-Linux so CI exercises them while other hosts still build.

- **Project infrastructure.** Continuous integration (`fmt` · `clippy` ·
  `build` · `test`, plus an examples smoke-test), contributor and security
  policies, dual MIT / Apache-2.0 licensing, and a development roadmap.

### Fixed

- **`k2-opt` inlining compile-time blow-up on cyclic call graphs.** Inlining
  accounting is now program-global: the recursion / global / per-caller inline
  budgets are threaded across every outer pass-manager iteration (previously the
  per-caller depth map was reborn each outer pass, so a recursive callee could be
  unrolled `RECURSION_BUDGET × OUTER_BUDGET` times and each copy reintroduced call
  sites the next pass unrolled again). The per-caller scan now resumes from the
  last inlined block and densifies once per caller instead of re-scanning the
  whole growing body and running `gc_unreachable_blocks` after every single
  inline, and the size gate measures the callee's *current* body (which may have
  grown on a cycle) rather than a stale summary. An 8-function mutual-recursion
  cycle that previously took ~10 s and produced ~5790 MIR blocks now compiles in
  under 0.1 s to ~129 blocks, byte-identical output. Inlining on the normal
  benchmarks is unaffected except a small, bounded reduction in recursive `fib`
  unrolling (still ~50% fewer executed instructions than Debug).
- **`MirProgram::verify` now checks all three `MakeSlice` operands.**
  `Rvalue::collect_locals` walked only `ptr`/`len`, so a dangling `offset` local
  in a `make_slice` slipped past the "no undefined local" invariant; it now walks
  `offset` too (the MIR pretty-printer also renders it).
- **Constant folding now masks comptime results like the VM.** A folded
  `Binary`/`Unary` whose result type is an unsized `comptime_int` stored into a
  sized local is now masked to the destination's width via the VM's `result_repr`
  fallback, matching the value the VM would compute at runtime exactly.

[Unreleased]: https://github.com/k2-lang/k2/commits/main
