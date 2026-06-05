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
| v0.3 | Canonical formatter + AST tooling (finalized in v0.13) | ✅ |
| v0.4 | Name resolution, scopes & module graph (HIR) | ⬜ |
| v0.5 | Type system & checker | ⬜ |
| v0.6 | The comptime engine & generics | ⬜ |
| v0.7 | MIR, monomorphization & safety checks | ⬜ |
| v0.8 | Bytecode VM — **programs run** | ⬜ |
| v0.9 | Optimizer & release mode — **proven fast** | ⬜ |
| v0.10 | Standard library & the `*System` capabilities | ⬜ |
| v0.11 | Concurrency: threads, sync & async | ⬜ |
| v0.12 | `build.k2` & the package/module system | ✅ |
| v0.13 | Tooling: formatter polish & language server | ✅ |

**The `v0.x` line is complete.** The k2 front end runs end to end — lex, parse,
resolve, type-check, comptime, MIR, optimize, run — and the canonical formatter
and the `k2c lsp` language server reuse those exact stages. What remains is the
*Beyond 0.13* work (native codegen, FFI, self-hosting), which deliberately steps
outside the pure-`std` rule.

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

## v0.3 — Canonical formatter + AST tooling ✅ (finalized in v0.13)

- ✅ `k2c fmt` — the one canonical layout for k2 source, built on the AST. Marked
  **stable** in v0.13; the `k2c lsp` formatting feature reuses the same engine.
- ✅ Idempotence and parse-print-parse round-trip tests across every example.
- ✅ `k2c ast` structured dump for tooling and golden tests.

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

## v0.12 — `build.k2` & the package/module system ✅

- ✅ `build.k2` *runs* — the build system **is** k2, with no second
  configuration language. `build(b: *Build)` is ordinary k2 executed on the VM
  with a `*Build` **capability** (the build-time analogue of `*System`) backed by
  `@build*` **recording** intrinsics that build a deterministic graph (no I/O, no
  real allocation — the comptime sandbox is honored). This is the faithful
  realization of "executed by the comptime engine" (noted in
  `docs/spec/08 §6.1`).
- ✅ `k2c build [step] [-Dkey=value …]`: runs `build(b)`, parses
  `-Doptimize`/`-Dtarget`/custom bool/string options, then executes the step —
  `install`/default **describes + validates** the DAG (native emission a
  documented no-op until post-0.13 codegen), `run` **builds + runs** the chosen
  executable through the VM, `test` **compiles + runs** the `test { ... }`
  blocks.
- ✅ A multi-file module graph: `.k2` **path imports** and **named modules**
  (`addModule`) resolve, type-check, monomorphize, lower, and run as one merged
  program (the std-injection move, generalized) — wired into `k2c run` too.
- ✅ A deterministic, reproducible `build.lock` (sorted graph + per-input content
  hashes; identical inputs → byte-identical lock). The offline/local realization
  of the §7.3 lockfile.
- ✅ Build options surfaced to programs via `@import("build_options")` — a
  synthesized comptime module whose `if (opts.flag)` dead branch the optimizer
  eliminates entirely.

## v0.13 — Tooling: formatter polish & language server ✅

- ✅ **`k2c fmt` finalized** as *the* canonical formatter — stable, idempotent,
  comment-preserving. The language server reuses the same `format_source` engine,
  so format-on-save and the CLI cannot diverge.
- ✅ A **pure-`std` JSON** value/parser/serializer (objects, arrays, strings with
  `\`-escapes and `\u` surrogate pairs, numbers, bool, null) that round-trips the
  LSP base protocol, with a depth guard and never-panic error handling.
- ✅ An **LSP server** (`k2c lsp`) over stdio: `Content-Length`-framed JSON-RPC,
  the full lifecycle (`initialize` → `ServerCapabilities`, `initialized`,
  `shutdown`, `exit`), and document sync (`didOpen`/`didChange` full **and**
  incremental/`didClose`) over an in-memory document store.
- ✅ **Features reusing the front-end crates** (zero re-implementation):
  `publishDiagnostics` (lex+parse+resolve+check with exact ranges/severities),
  `hover` (per-occurrence type + def kind), `definition` (occurrence→definition,
  fields/variants via the member table), `completion` (scope-aware identifiers +
  member-after-`.`), and `formatting` (a single full-document `TextEdit`).
- ✅ **UTF-16 ↔ scalar-offset position mapping**, unit-tested on multi-byte and
  astral (surrogate-pair) text — ranges are exact.
- ✅ **Editor-integration notes** (`docs/lsp.md`) and the updated `docs/tooling.md`
  status.
- ✅ **Final integration pass:** every example runs (`hello`/`errors`/
  `allocators`/`generic_list`/`concurrency` via `k2c run`, `build.k2` via
  `k2c build`), the whole `cargo test --workspace` suite is green, and the
  optimizer benchmark still shows the ~2× speedup (`k2c bench`).

---

## v0.14 – v0.30 — native code, depth & maturity

The `v0.x` line through v0.13 delivered a complete toolchain over a bytecode
VM. The v0.14–v0.30 line takes k2 to a **native, production-grade** language —
*still pure `std`, still offline*. The key realization: native code generation
does **not** require Cranelift/LLVM. We emit our **own** machine code (x86-64
then aarch64) and our **own** ELF object/executable writer, in pure Rust `std`,
and the `*System` capabilities become raw Linux **syscalls** (no libc). Because
CI runs on x86-64 Linux, every native milestone is verified by *actually
running the emitted binary*.

Legend: ✅ done · 🚧 in progress · ⬜ not started.

| Version | Milestone | Track |
| --- | --- | --- |
| v0.14 | Native backend foundation (x86-64 encoder + ELF writer) | native |
| v0.15 | Native backend: full language + register allocator + SysV ABI | native |
| v0.16 | Native runtime: `*System` via raw Linux syscalls (no libc) | native |
| v0.17 | Native optimization & ReleaseFast — native ≫ VM (benchmarked) | native |
| v0.18 | aarch64 backend + cross-compilation (`-Dtarget`) | native |
| v0.19 | C interop & FFI: `extern`/`export`, SysV calls, system linker | interop |
| v0.20 | Diagnostics polish & error-return traces | quality |
| v0.21 | Language depth: packed structs, bit-fields, `align`, `@Vector`/SIMD | language |
| v0.22 | Stdlib data structures: HashMap, sort, bignum, allocators, Unicode | stdlib |
| v0.23 | Stdlib OS/IO/net: `std.fs`, `std.os`, `std.time`, `std.net` (syscalls) | stdlib |
| v0.24 | Test runner & coverage: `k2c test`, coverage, fuzz helpers | tooling |
| v0.25 | Package manager: deps, lockfile, semver, offline-reproducible | tooling |
| v0.26 | LSP completeness: references/rename/signature-help/semantic-tokens | tooling |
| v0.27 | Debug info: DWARF in the native backend; gdb-debuggable | tooling |
| v0.28 | Doc generator (`k2c doc`) + doc-tests ✅ | tooling |
| v0.29 | Self-hosting groundwork: a k2-written front-end, run by the toolchain | self-host |
| v0.30 | 1.0 readiness: conformance suite, stability pass, full integration | maturity |

### Native track (v0.14–v0.18)

- **v0.14** — Lower MIR → a low-level codegen IR; a hand-written **x86-64
  instruction encoder** (machine-code bytes) and an **ELF64 writer**, all pure
  `std`. `k2c run-native` / `k2c build-native` produce and run a real
  Linux/x86-64 executable for a working subset (hello world prints via a
  `write` syscall).
- **v0.15** — Full language coverage to native (functions, calls, structs,
  slices, optionals/error-unions, all control flow, the safety checks), a
  **register allocator**, and the **System V AMD64** calling convention. The
  examples compile to native and run.
- **v0.16** — The `*System` capabilities as raw syscalls — `write`/`read`,
  `mmap`-backed heap, `clock_gettime`, `getrandom`, `exit` — so native binaries
  do real I/O with **no libc**. Differential: native output ≡ VM output.
- **v0.17** — Machine-level optimization (peephole, better register allocation,
  instruction selection) and a native **ReleaseFast**; a benchmark proving the
  native binary is dramatically faster than the VM.
- **v0.18** — A second backend (**aarch64**) sharing the codegen IR, and
  first-class **cross-compilation** via `-Dtarget=<triple>`.

### Breadth (v0.19–v0.28)

C **FFI** (`extern`/`export`, ABI-correct layout, calling libc via the system
linker); **rich diagnostics** with caret underlines, multi-span notes,
suggestions, and `@errorReturnTrace`; **language depth** (packed structs,
bit-fields, `align`, `@Vector`/SIMD); a **deeper stdlib** (HashMap, sort,
bignum, page/stack allocators, Unicode, `std.fs`/`std.os`/`std.time`/`std.net`
over syscalls); a project **test runner** with coverage; an offline-reproducible
**package manager**; **LSP completeness** (references, rename, signature help,
inlay hints, semantic tokens, code actions, cross-file); **DWARF** debug info;
and a **documentation generator**.

### Maturity (v0.29–v0.30)

- **v0.29 — Self-hosting groundwork.** A k2-written lexer (and parser) compiled
  and run by the toolchain and **differentially tested against the Rust
  front-end** — a proof of expressiveness. (k2 still does not *commit* to full
  self-hosting; the Rust implementation stays the reference until a self-hosted
  one clearly wins on robustness.)
- **v0.30 — 1.0 readiness.** A spec-**conformance** test suite, a stability
  pass over the language surface, and a full integration sweep: every example
  runs on **both** the VM and the native backend, the whole suite is green, and
  the optimizer's wins are re-verified. This closes the extended roadmap.

## Beyond 0.30

Full multi-platform self-hosting, an LLVM-grade optimizing middle-end,
Windows/macOS targets, an incremental/cached compilation cache, and a richer
package ecosystem — pursued once the language surface is stable at 1.0.

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
