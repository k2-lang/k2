# k2 Roadmap

> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

This roadmap is **honest about where k2 is today**: the language is fully
*designed* — the locked charter, the EBNF grammar, and ten normative spec
chapters all exist under [`docs/`](docs/) — but the *implementation* has only
just begun. The compiler is a Rust workspace, and exactly one phase of the
front-end is working: the **lexer**.

We build front-to-back from a locked design, so each milestone is a vertical
slice that turns more of the spec into running code. Nothing below is shipped
until its spec chapter and its tests agree.

Legend: ✅ done · 🚧 in progress · ⬜ not started.

---

## Where we are now

| Component | State | Notes |
| --- | --- | --- |
| Language design (charter, grammar, spec §01–§10) | ✅ | Locked; co-normative with the implementation. |
| `k2-lexer` | ✅ | Spec-faithful tokenizer with error recovery and a real test suite. |
| `k2-syntax` (AST + spans) | 🚧 | Node definitions and span helpers exist; not yet produced by a parser. |
| `k2c` driver | 🚧 | Hand-rolled CLI; only the `tokenize`/`lex` subcommand is wired up. |
| Parser | ⬜ | Next up (see v0.2). |
| Everything below the parser | ⬜ | Specified, not implemented. |

---

## v0 — Front-end skeleton (current)

The goal of v0 is a working **source → typed AST** front-end with great
diagnostics, all in pure stable Rust with no third-party crates.

### v0.1 — Lexer ✅ (shipped)

- ✅ Complete tokenizer for the locked 35-keyword set, `@builtins`, escaped
  identifiers, all literal forms (int radixes, decimal/hex floats, char,
  string, multiline string), and the full operator/punctuation table with
  maximal munch.
- ✅ `!` lexes strictly as the error-union constructor; `and`/`or`/`not` are
  keywords (never a symbolic boolean not).
- ✅ Non-panicking error recovery: lexical errors become `Error` tokens so later
  phases can report precisely.
- ✅ `k2c lex <file>` / `k2c tokenize -` driver subcommand.

### v0.2 — Parser 🚧 (current focus)

- ⬜ Recursive-descent parser producing the `k2-syntax` AST for the whole
  grammar in `docs/grammar.ebnf`.
- ⬜ Postfix-modifier type grammar (`?T`, `!T`, `E!T`, `*T`, `*const T`, `[]T`,
  `[N]T`).
- ⬜ Error-recovering parse with source-spanned diagnostics.
- ⬜ `k2c parse <file>` (AST dump) and a round-trip / pretty-print check.

### v0.3 — HIR, name resolution, module graph ⬜

- ⬜ Lower the AST to a typed HIR.
- ⬜ Resolve `@import`, build the module/namespace graph, resolve names and
  predeclared types.

**Exit criteria for v0:** every program under `examples/` parses and lowers to
HIR with no errors, and the diagnostics are clear and well-spanned.

---

## v0.4 — Semantic analysis & type system ⬜

- ⬜ Bidirectional type inference; `@TypeOf` resolution.
- ⬜ Optionals (`?T`), error unions (`E!T` / `!T`), slices, arrays, pointers.
- ⬜ Capability types resolved (`*System`, `Allocator`, and the `sys.*` handles).
- ⬜ Exhaustiveness checking for `switch` (including over error sets and enums).

---

## v0.5 — The comptime engine ⬜

The single metaprogramming mechanism, and the heart of generics.

- ⬜ Sandboxed comptime interpreter over ordinary k2 (no I/O, no runtime
  allocation, guaranteed to terminate).
- ⬜ `type` as a first-class comptime value; generics as `fn(comptime T: type) type`,
  instantiated and cached per distinct argument.
- ⬜ Reflection: `@typeInfo` / `@Type` round-trip, `@hasField`, `@field`,
  `@sizeOf`, `@alignOf`.
- ⬜ `@compileError` / `@compileLog` with precise, source-located diagnostics.
- ⬜ `inline for` / `inline` over comptime-known sequences.

---

## v0.6 — MIR, monomorphization & safety checks ⬜

- ⬜ A k2-native **MIR**: monomorphized, comptime-folded, backend-agnostic.
- ⬜ Safety-check insertion for Debug/ReleaseSafe: bounds, integer overflow,
  narrowing-cast (`@intCast`), and `unreachable` checks.
- ⬜ First pass of **comptime leak/escape analysis** — flag obvious allocator
  misuse (a value allocated and returned without ownership transfer, a missing
  paired free in a simple scope) as a compile-time diagnostic.

---

## v0.7 — Cranelift debug backend ⬜

- ⬜ Lower MIR to native code via `cranelift-codegen` for near-instant Debug
  builds.
- ⬜ `k2 run <file>` and `k2 build` (Debug) compile and execute real programs.
- ⬜ Full safety toolkit live: bounds/overflow checks plus the runtime-checked
  `GeneralPurposeAllocator` (leak / double-free / use-after-free detection).
- ⬜ The minimum runtime shim that constructs `*System` and dispatches to `main`.

**This is the first milestone where `examples/hello.k2` actually runs.**

---

## v0.8 — LLVM release backend ⬜

- ⬜ Lower the *same* MIR to LLVM IR (via `inkwell`/`llvm-sys`) for optimized
  native code.
- ⬜ Build modes complete: **Debug** (Cranelift), **ReleaseSafe** (LLVM + checks),
  **ReleaseFast** (LLVM, checks stripped).
- ⬜ Verify zero-cost abstractions: capability indirections devirtualize when
  monomorphic; generated code matches hand-written equivalents.
- ⬜ First-class **cross-compilation**: `-Dtarget=<triple>`, bundled libc
  headers/stubs for common targets.

---

## v0.9 — C interop & FFI ⬜

- ⬜ `extern` / `export` function and variable interop.
- ⬜ Integrated C-translation path so C headers are usable directly.
- ⬜ ABI-correct struct layout validated against `@sizeOf` / `@alignOf`.

---

## v0.10 — Standard library ⬜

The stdlib that *never allocates on your behalf*. Tracked against
[`docs/spec/10-standard-library.md`](docs/spec/10-standard-library.md).

- ⬜ Allocators: `GeneralPurposeAllocator`, `FixedBufferAllocator`,
  `ArenaAllocator`, page allocator.
- ⬜ Core containers: `ArrayList`, hash map, and friends — all allocator-taking.
- ⬜ The capability surfaces behind `*System`: `io`, `heap`, `clock`, `random`,
  `env`, `net`.
- ⬜ Formatting, `testing` helpers (`expectError`, the testing allocator), and
  string/slice utilities.

---

## v0.11 — Concurrency ⬜

Library-provided over OS threads; no built-in runtime. See
[`docs/spec/09-concurrency.md`](docs/spec/09-concurrency.md).

- ⬜ `ThreadPool` / `Executor` capabilities (passed, never global).
- ⬜ `Mutex`, atomics, and other explicit synchronization primitives.
- ⬜ Colorless, stackless `async`/`await` lowered at compile time, with
  caller-owned `Frame` storage and an event loop obtained from `*System`.

---

## v0.12 — Package manager & `build.k2` ⬜

- ⬜ `build.k2` executed by the comptime engine — the build system *is* k2
  itself, with no second configuration language.
- ⬜ Dependency fetching, lockfiles, and reproducible builds.
- ⬜ Build options surfaced to programs via `@import("build_options")`.

---

## v0.13 — Tooling: LSP & formatter ⬜

- ⬜ `k2 fmt` — a canonical formatter (one obvious way to lay out code).
- ⬜ A language server: diagnostics, go-to-definition, hover, and completion,
  reusing the front-end crates.
- ⬜ Editor integrations.

---

## Self-hosting? — an open question, deliberately

k2's compiler is implemented in **Rust** today, and that is a stated design
choice (memory- and data-race-safe compiler, mature Cranelift/LLVM crates,
clean ADTs for the IRs). Unlike Zig, **k2 does not commit to self-hosting.**

Once the language is capable enough (post-v0.10), we will evaluate a self-hosted
front-end as an experiment and a proof of expressiveness — but the Rust
implementation remains the reference unless and until a self-hosted compiler
clearly wins on robustness and maintainability. No promises here; correctness of
the compiler outranks the romance of self-hosting.

---

## 1.0 goals

k2 reaches **1.0** when:

- ⬜ The whole language in `docs/spec/§01–§10` is implemented, with the spec and
  the compiler co-normative and tested against each other.
- ⬜ All three build modes (Debug/Cranelift, ReleaseSafe/LLVM, ReleaseFast/LLVM)
  are solid, and cross-compilation works for the common target triples.
- ⬜ Zero-cost abstractions are verified: capability indirection and generics
  compile to hand-written-equivalent machine code.
- ⬜ The standard library, package manager, formatter, and LSP are usable for
  real projects.
- ⬜ The language surface is **stable** — a `1.0` program keeps compiling. After
  1.0, breaking changes follow a deprecation policy.

Until then, **expect breakage**: k2 is pre-alpha, the surface may shift, and the
only guarantee is that the lexer turns your `.k2` files into tokens.

---

*Dates are intentionally omitted. Milestones ship when they are correct, not
when a calendar says so — that is what "zero waste" means for the project, too.*
