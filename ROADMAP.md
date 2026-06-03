# k2 Roadmap

> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

This roadmap drives k2 from a designed language with a working lexer to a
**complete, self-contained toolchain that actually runs k2 programs** — parser,
name resolution, type system, the comptime engine, an optimizing IR, and a
bytecode virtual machine — plus a standard library, a build system written in
k2, and developer tooling (formatter + language server).

## Implementation constraint: pure `std`, fully offline

The whole toolchain is built on the Rust **standard library only** — no
third-party crates — so it builds, tests, and runs with no network access and a
single `cargo` invocation. This is a hard project rule, and it shapes the
backend strategy:

- The execution engine for the `v0.x` series is a **k2 bytecode compiler + a
  register-based virtual machine**, written in pure `std`. This is what makes
  k2 programs *run*, and it is where the "fast" pillar is proven (via an
  optimizing pass and a benchmark harness).
- **Native code generation** through Cranelift/LLVM is real and desirable, but
  it requires external crates, so it is deliberately **post-0.13 future work**
  (see *Beyond 0.13*). Nothing in the `v0.x` line depends on it.

We build front-to-back from the locked design in [`docs/`](docs/); each
milestone is a vertical slice that turns more of the spec into running,
tested code. A version ships only when it builds clean (`fmt` + `clippy -D
warnings`), its tests pass, and CI is green — then it is tagged and pushed.

Legend: ✅ done · 🚧 in progress · ⬜ not started.

---

## Status overview

| Version | Milestone | State |
| --- | --- | --- |
| v0.1 | Lexer + driver | ✅ |
| v0.2 | Parser → AST | ⬜ |
| v0.3 | Canonical formatter + AST tooling | ⬜ |
| v0.4 | Name resolution, scopes & module graph (HIR) | ⬜ |
| v0.5 | Type system & checker | ⬜ |
| v0.6 | The comptime engine & generics | ⬜ |
| v0.7 | MIR, monomorphization & safety checks | ⬜ |
| v0.8 | Bytecode VM — **programs run** | ⬜ |
| v0.9 | Optimizer & release mode — **proven fast** | ⬜ |
| v0.10 | Standard library & the `*System` capabilities | ⬜ |
| v0.11 | Concurrency: threads, sync & async | ⬜ |
| v0.12 | `build.k2` & the package/module system | ⬜ |
| v0.13 | Tooling: formatter polish & language server | ⬜ |

---

## v0.1 — Lexer + driver ✅

- ✅ Complete tokenizer for the locked keyword set, `@builtins`, escaped
  identifiers, every literal form, and the full operator/punctuation table with
  maximal munch.
- ✅ `!` lexes strictly as the error-union constructor; `and`/`or`/`not` are
  keywords. Non-panicking error recovery via `Error` tokens.
- ✅ `k2c lex`/`tokenize` driver subcommand. Extensive unit tests.

## v0.2 — Parser → AST ⬜

- Recursive-descent parser (`k2-parse`) covering the whole grammar in
  [`docs/grammar.ebnf`](docs/grammar.ebnf): items, statements, the full
  expression grammar with correct precedence, blocks-as-expressions, `if` /
  `while` / `for` / `switch`, `struct` / `enum` / `union` / `error` type
  declarations, and the postfix-modifier type grammar (`?T`, `!T`, `E!T`, `*T`,
  `*const T`, `[]T`, `[N]T`).
- Error-recovering parse with precise, source-spanned diagnostics.
- `k2c parse <file>` dumps the AST; an S-expression form supports round-trip
  testing.

## v0.3 — Canonical formatter + AST tooling ⬜

- `k2c fmt` — the one canonical layout for k2 source, built on the AST.
- Idempotence and parse-print-parse round-trip tests across every example.
- `k2c ast` structured dump for tooling and golden tests.

## v0.4 — Name resolution, scopes & module graph (HIR) ⬜

- Lower the AST to a resolved **HIR**: every identifier bound to a declaration,
  every scope and shadowing rule enforced.
- `@import` resolution and a project module/namespace graph.
- Predeclared types and builtins in scope; clear "unresolved name" diagnostics.

## v0.5 — Type system & checker ⬜

- A real type representation and a bidirectional checker with local inference;
  `@TypeOf` resolution.
- Optionals (`?T`), error unions (`E!T` / `!T`), pointers, slices, arrays,
  structs, enums, and the capability types (`*System`, `Allocator`, `sys.*`).
- `switch` exhaustiveness over enums and error sets. `k2c check <file>`.

## v0.6 — The comptime engine & generics ⬜

The single metaprogramming mechanism, and the heart of generics.

- A sandboxed comptime interpreter over ordinary k2 — no I/O, no runtime
  allocation, guaranteed to terminate.
- `type` as a first-class comptime value; generics as
  `fn(comptime T: type) type`, instantiated and cached per distinct argument.
- Reflection: `@typeInfo` / `@Type` round-trip, `@hasField`, `@field`,
  `@sizeOf`, `@alignOf`; `@compileError` / `@compileLog`; `inline for`.

## v0.7 — MIR, monomorphization & safety checks ⬜

- A backend-agnostic **MIR**: monomorphized and comptime-folded.
- Safety-check insertion for Debug/ReleaseSafe: bounds, integer overflow,
  narrowing-cast (`@intCast`), and `unreachable`.
- First pass of comptime **leak/escape analysis** flagging obvious allocator
  misuse as a compile-time diagnostic.

## v0.8 — Bytecode VM — programs run ⬜

- A bytecode compiler lowering MIR to a compact instruction set, and a
  register-based **virtual machine** that executes it — all pure `std`.
- The runtime shim that constructs the root `*System` and dispatches `main`.
- `k2c run <file>` compiles and executes real programs.

**This is the milestone where `examples/hello.k2` actually runs and prints.**

## v0.9 — Optimizer & release mode — proven fast ⬜

- An optimizing pass over MIR/bytecode: constant folding, dead-code
  elimination, copy propagation, and devirtualization/inlining of monomorphic
  capability calls.
- Build modes: **Debug** (checks on), **ReleaseSafe** (optimized + checks),
  **ReleaseFast** (checks stripped).
- A reproducible **benchmark harness** demonstrating the speedups — the "zero
  waste / fast" pillar, made measurable.

## v0.10 — Standard library & the `*System` capabilities ⬜

Tracked against [`docs/spec/10-standard-library.md`](docs/spec/10-standard-library.md);
the stdlib that *never allocates on your behalf*.

- Allocators: `FixedBufferAllocator`, `ArenaAllocator`, and a runtime-checked
  `GeneralPurposeAllocator` (leak / double-free / use-after-free detection).
- Core containers: `ArrayList`, hash map, and friends — all allocator-taking.
- The capability surfaces behind `*System`: `io`, `heap`, `clock`, `random`,
  `env`. Formatting and `testing` helpers (`expectError`, testing allocator).

## v0.11 — Concurrency: threads, sync & async ⬜

Library-provided over OS threads; no built-in runtime. See
[`docs/spec/09-concurrency.md`](docs/spec/09-concurrency.md).

- `ThreadPool` / `Executor` capabilities (passed, never global), `Mutex`, and
  atomics — explicit synchronization primitives.
- Colorless, stackless `async`/`await` lowered at compile time, with
  caller-owned `Frame` storage and an event loop obtained from `*System`.

## v0.12 — `build.k2` & the package/module system ⬜

- `build.k2` executed by the comptime engine — the build system *is* k2 itself,
  with no second configuration language. `k2c build`.
- A multi-file module/package graph, lockfile, and reproducible builds.
- Build options surfaced to programs via `@import("build_options")`.

## v0.13 — Tooling: formatter polish & language server ⬜

- `k2 fmt` finalized as the canonical formatter.
- A **language server** (`k2c lsp`) over the LSP base protocol: diagnostics,
  hover, go-to-definition, and completion — reusing the front-end crates, in
  pure `std`.
- Editor-integration notes. Final integration pass: every example runs, the
  whole suite is green, and the optimizer's wins are re-verified.

---

## Beyond 0.13 — native codegen, FFI, self-hosting

Deferred precisely because it breaks the pure-`std` rule or is better done once
the language is complete:

- **Native backends.** Lower the *same* MIR to native code via Cranelift (fast
  debug builds) and LLVM (optimized release) — first-class cross-compilation
  with `-Dtarget=<triple>`. Requires external crates.
- **C interop & FFI.** `extern` / `export`, direct use of C headers, and
  ABI-correct layout validated against `@sizeOf` / `@alignOf`.
- **Self-hosting — an open question, deliberately.** k2's compiler is in Rust
  by choice (a memory- and data-race-safe compiler, clean ADTs for the IRs).
  Unlike Zig, **k2 does not commit to self-hosting.** We will evaluate a
  self-hosted front-end as a proof of expressiveness, but the Rust
  implementation stays the reference unless a self-hosted one clearly wins on
  robustness. Correctness of the compiler outranks the romance of self-hosting.

## 1.0 goals

k2 reaches **1.0** when the whole language in `docs/spec/§01–§10` is implemented
and co-normative with the compiler; the standard library, build system,
formatter, and language server are usable for real projects; the optimizer's
zero-cost-abstraction claims are verified; and the language surface is stable —
a `1.0` program keeps compiling, with breaking changes gated behind a
deprecation policy.

Until then, **expect breakage**: k2 is pre-alpha and the surface may shift.

---

*Dates are intentionally omitted. Milestones ship when they are correct, not
when a calendar says so — that is what "zero waste" means for the project, too.*
