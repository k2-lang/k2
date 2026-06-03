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
