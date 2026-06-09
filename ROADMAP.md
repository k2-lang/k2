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
| v0.2 | Parser → AST | ✅ |
| v0.3 | Canonical formatter + AST tooling (finalized in v0.13) | ✅ |
| v0.4 | Name resolution, scopes & module graph (HIR) | ✅ |
| v0.5 | Type system & checker | ✅ |
| v0.6 | The comptime engine & generics | ✅ |
| v0.7 | MIR, monomorphization & safety checks | ✅ |
| v0.8 | Bytecode VM — **programs run** | ✅ |
| v0.9 | Optimizer & release mode — **proven fast** | ✅ |
| v0.10 | Standard library & the `*System` capabilities | ✅ |
| v0.11 | Concurrency: threads, sync & async | ✅ |
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

## v0.2 — Parser → AST ✅

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

## v0.4 — Name resolution, scopes & module graph (HIR) ✅

- Lower the AST to a resolved **HIR**: every identifier bound to a declaration,
  every scope and shadowing rule enforced.
- `@import` resolution and a project module/namespace graph.
- Predeclared types and builtins in scope; clear "unresolved name" diagnostics.

## v0.5 — Type system & checker ✅

- A real type representation and a bidirectional checker with local inference;
  `@TypeOf` resolution.
- Optionals (`?T`), error unions (`E!T` / `!T`), pointers, slices, arrays,
  structs, enums, and the capability types (`*System`, `Allocator`, `sys.*`).
- `switch` exhaustiveness over enums and error sets. `k2c check <file>`.

## v0.6 — The comptime engine & generics ✅

The single metaprogramming mechanism, and the heart of generics.

- A sandboxed comptime interpreter over ordinary k2 — no I/O, no runtime
  allocation, guaranteed to terminate.
- `type` as a first-class comptime value; generics as
  `fn(comptime T: type) type`, instantiated and cached per distinct argument.
- Reflection: `@typeInfo` / `@Type` round-trip, `@hasField`, `@field`,
  `@sizeOf`, `@alignOf`; `@compileError` / `@compileLog`; `inline for`.

## v0.7 — MIR, monomorphization & safety checks ✅

- A backend-agnostic **MIR**: monomorphized and comptime-folded.
- Safety-check insertion for Debug/ReleaseSafe: bounds, integer overflow,
  narrowing-cast (`@intCast`), and `unreachable`.
- First pass of comptime **leak/escape analysis** flagging obvious allocator
  misuse as a compile-time diagnostic.

## v0.8 — Bytecode VM — programs run ✅

- A bytecode compiler lowering MIR to a compact instruction set, and a
  register-based **virtual machine** that executes it — all pure `std`.
- The runtime shim that constructs the root `*System` and dispatches `main`.
- `k2c run <file>` compiles and executes real programs.

**This is the milestone where `examples/hello.k2` actually runs and prints.**

## v0.9 — Optimizer & release mode — proven fast ✅

- An optimizing pass over MIR/bytecode: constant folding, dead-code
  elimination, copy propagation, and devirtualization/inlining of monomorphic
  capability calls.
- Build modes: **Debug** (checks on), **ReleaseSafe** (optimized + checks),
  **ReleaseFast** (checks stripped).
- A reproducible **benchmark harness** demonstrating the speedups — the "zero
  waste / fast" pillar, made measurable.

## v0.10 — Standard library & the `*System` capabilities ✅

Tracked against [`docs/spec/10-standard-library.md`](docs/spec/10-standard-library.md);
the stdlib that *never allocates on your behalf*.

- Allocators: `FixedBufferAllocator`, `ArenaAllocator`, and a runtime-checked
  `GeneralPurposeAllocator` (leak / double-free / use-after-free detection).
- Core containers: `ArrayList`, hash map, and friends — all allocator-taking.
- The capability surfaces behind `*System`: `io`, `heap`, `clock`, `random`,
  `env`. Formatting and `testing` helpers (`expectError`, testing allocator).

## v0.11 — Concurrency: threads, sync & async ✅

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
| v0.29 | Self-hosting groundwork: a k2-written front-end, run by the toolchain ✅ | self-host |
| v0.30 | 1.0 readiness: conformance suite, stability pass, full integration ✅ | maturity |

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

- **v0.29 — Self-hosting groundwork. ✅** A k2-written **lexer**
  ([`selfhost/lexer.k2`](selfhost/lexer.k2)) — a faithful port of the Rust
  reference lexer (`crates/k2-lexer`) — compiled and run by the toolchain on
  **both** the bytecode VM and the **native** x86-64 backend, and
  **differentially tested against the Rust front-end** over a broad corpus
  (every token kind, maximal munch, all literal forms, comment/doc/multiline
  boundaries, multibyte column tracking, BOM/NUL, and the example programs). The
  lexer is allocation-free and emits a canonical `line:col Kind byteLen`
  signature per token; `(line, col, byteLen)` pins each lexeme's exact span in
  the shared source, so identical signatures prove identical tokenization. The
  effort flushed out — and fixed — **two latent compiler bugs** that only a real
  k2 program of this size exercises: a `switch` prong that is a named value
  `const` silently never matched (MIR `switch` lowering dropped it), and
  widening a value loaded directly from a slice index used the *coerced* result
  type to size the native load (reading 4 bytes for a `[]const u8` element). Both
  fixes live in shared MIR lowering, so the VM and native backend agree. (k2
  still does not *commit* to full self-hosting; the Rust implementation stays the
  reference until a self-hosted one clearly wins on robustness.)
- **v0.30 — 1.0 readiness. ✅** A spec-**conformance** test suite (a committed
  `conformance/` corpus of small programs spanning lexical structure → the
  standard library, each run and checked against its captured output on the VM,
  and on the **native** backend wherever the two agree byte-for-byte); a
  **stability pass** over the language surface; and a full **integration sweep**
  (`crates/k2c/tests/integration_examples.rs`) pinning the authoritative
  example→backend matrix: every runnable example runs deterministically on the
  VM, the native-capable ones match the VM byte-for-byte, and the rest are
  *cleanly refused* by the native subset (never miscompiled). The optimizer's
  wins are re-verified (`bench_baseline.rs`), and the whole suite is green. This
  closes the extended roadmap.

---

## v0.31 — Tagged-union runtime values `union(enum)` ✅

The single largest item carried in *Beyond 0.30* — the only language construct
the front-end fully understood but the backends *cleanly refused* — is now a
running feature. A `union(enum)` is a **sum type**: a value that is exactly one
of its variants at a time, with a discriminating tag. This milestone makes such
values **construct, flow, and `switch` at run time**, end to end, on both
backends.

- **Layout (single source of truth).** A union is laid out like a generalized
  error union: a discriminant (the inferred enum tag — `enum_tag_bits(n)` bits,
  unsigned, so a 1-byte tag for ≤256 variants) at `+0`, then a payload area sized
  to the **largest** variant and aligned to the **strictest**. The rule lives in
  `k2_types::reflect::layout_depth` (which feeds `@sizeOf`/`@alignOf`/`@offsetOf`)
  and is mirrored byte-for-byte in `k2_codegen::layout`, so the comptime-folded
  constants and the native byte image agree.
- **One constructor.** All construction — the payload-carrying `.{ .circle = r }`
  and the bare `.point` form — lowers to a single MIR rvalue, `MakeUnion { variant,
  payload, ty }` (parallel to `MakeOk`/`MakeSome`). The VM realizes it as
  `Value::Enum { tag, payload }`; native writes the tag word plus the payload.
- **Switch with payload capture.** `switch (u) { .circle => |r| …, .point => … }`
  reads the union's tag into the existing `SwitchInt`, then binds each arm's
  capture to the **active variant's payload** (a `Proj::Payload` at the variant
  type) — reusing the optional/error-union capture machinery, generalized to N
  variants. Exhaustiveness is enforced by the type checker; an `else` arm covers
  the rest.
- **Tagless variants.** The parser now accepts a payload-less union variant
  (`point,`), matching the spec's `union(enum)` example; its payload is `void`.
- **Backends & the honest boundary.** The **VM** supports scalar *and* aggregate
  (`struct`) payloads. The **native x86-64** backend supports scalar and `void`
  payloads with output byte-identical to the VM (verified across Debug,
  ReleaseSafe, and ReleaseFast); a variant with an **aggregate** payload, or a
  bare untagged `union {…}`, is **cleanly refused** by native (a non-zero exit
  naming the VM fallback), never miscompiled. Broadening native to aggregate
  union payloads (the by-register SysV-ABI spill) is incremental future work.
- **Coverage.** `examples/unions.k2` (native-capable, in the integration
  parity set); `conformance/02-types/union_tagged.k2` (native-marked) and
  `union_payload_struct.k2` (VM-only, aggregate payload); unit tests for the
  layout formula (`k2_codegen::layout`), the tagless-variant parse
  (`k2-parse`), and the both-backends / native-refusal contracts
  (`crates/k2c/tests/cli.rs`). Implementing this also confirmed the existing
  enum-`switch` dispatch and `DiscrKind::Union` discriminant read were already
  union-ready — only payload storage and capture were missing.
- **Pre-existing bugs flushed out (and fixed) along the way** — real miscompiles
  that only a feature exercising aggregate layout and value coercion surfaces:
  1. **A 9–15 byte aggregate passed BY VALUE corrupted an adjacent frame slot.**
     Such a value is passed in a SysV register pair and received with two 8-byte
     stores; the home was reserved at `max(size, 8)`, so the second store spilled
     past it. Now padded to `round_up(size, 8)`. This fixes plain `struct`
     parameters too — pinned by `conformance/02-types/struct_by_value_abi.k2`.
  2. **Union-literal construction relied on the literal's span type, not its
     destination.** Three miscompiles shared this root cause: a union coerced into
     `?U`/`E!U` (a union is not transparent through an optional, so it needs an
     explicit `MakeSome`/`MakeOk` wrap); a payload-less `.variant` coerced the same
     way (read back as `null`); and a union literal NESTED as another union's
     variant payload (its type comes from the outer variant, so it lowered to a
     tagless struct). Construction now derives the union target from the
     DESTINATION type first, then the span — fixing all three uniformly. Pinned by
     `conformance/02-types/union_in_optional.k2` and `union_nested.k2`, and the
     `*_is_not_miscompiled` cli tests. Found by systematic edge-case testing of the
     new feature, exactly as the v0.29 self-hosting work flushed out latent bugs.

---

## v0.32 — `union(enum)` reflection ✅

`@typeInfo(U).Union` now reports a union's **`tag_type`** (its inferred
discriminant integer) and **`fields`** — one descriptor per variant carrying the
variant's `name` and payload `type` (reusing the `StructField` descriptor, since
a variant *is* `name : type`; a payload-less variant's `type` is `void`).
Previously the union descriptor was empty. It composes with the rest of the
comptime surface exactly as struct/enum reflection does — `@typeInfo(U) != .Union`,
`inline for (…Union.fields)`, `@sizeOf(field.type)`, sizing an array by
`…fields.len` — and folds to runtime constants, so it is native-capable. Pinned
by `conformance/07-comptime/union_reflection.k2` (native-marked). This makes a
union introspectable by the same generic code that already walks structs and
enums (serializers, ABI shims, trait-style checks).

---

## v0.33 — Lexical reconciliation ✅

Spec §1.3 relaxed so a lone `\r` (an old-style Mac line ending) is **whitespace**,
matching the reference lexer (it was previously called a lexical error). Closes the
one deferred lexical gap; pinned by `k2-lexer`'s
`lone_carriage_return_is_whitespace`.

## v0.34 — `struct` field defaults ✅

A `struct` field with a `= default` is now honored when an initializer omits it,
where it previously read back `undefined` (and an empty `C{}` — which parses as an
empty tuple — *faulted*). Construction indexes each struct's declared field
defaults (keyed by its defining span, mirroring the existing value-const index) and
lowers the default expression into any omitted field; an empty struct initializer
takes the same all-defaults path. Every default kind works (int, bool, float,
`[]const u8`), VM and native. Pinned by
`conformance/02-types/struct_field_defaults.k2` (native-marked) and a cli test.
This was a pre-existing silent miscompile, surfaced by the post-v0.31 systematic
edge-case sweep.

## v0.35 — Explicit `enum` values ✅

`enum(u8) { ok = 0, busy = 10, gone = 200 }` now honors its explicit integer values
(spec §9): each `EnumVariant` carries its tag VALUE (an explicit `= N`, else the
previous + 1), and that value — not the declaration index — flows through
construction, `switch` dispatch, `@intFromEnum`/`@enumFromInt` (runtime and
comptime), and `@typeInfo` reflection. A bare enum is unchanged (0, 1, 2 …). The
declaration *index* stays distinct (it still orders reflection and tags a
`union(enum)`); the two diverge only under explicit values. Pinned by
`conformance/02-types/enum_explicit_values.k2` (native-marked) and a cli test. The
last sibling of the v0.31 edge-case sweep; both this and v0.34 were pre-existing
silent miscompiles a real feature exercise flushed out.

## v0.40 — Standard-library depth + `++` strings ✅

A broad, verified `std` expansion — all pure or single-allocator, each pinned by a
`conformance/10-stdlib` case and (for the algorithmic ones) checked against
published vectors or identities: integer⇄string `std.fmt` (parse/format, radix,
`i64::MIN`), `std.ascii` classifiers, generic `std.mem` search, more `std.math`
(`isPrime`/`modPow`/alignment/digit ops), `std.str` case/search, **`std.hash`**
(CRC-32/Adler-32/FNV-1a, vector-checked), **`std.sort`** `heapSort`/`mergeSort`,
**`std.mathf`** (`sqrt`/`exp`/`sin`/`cos`/`tan`/`ln`/`pow` from scratch, ≈1e-12),
and **`std.bits`** (byte-swap/rotate/single-bit). Eight new end-to-end example
programs (word-count, base conversion, prime sieve, RLE, Caesar cipher, statistics,
anagram, geometry, config parser) each have an exact-output test. Alongside,
**`++` string concatenation** is fixed — it folded to `undef` and trapped on use;
the checker now records the folded bytes by span and the MIR materializes a
`Const::Str`.

## v0.39 — No silent builtins ✅

Every `@builtin` is validated against a `KNOWN_BUILTINS` allowlist (the typed
builtins plus the raw/intrinsic and `@build*` graph builtins the std and build
system use). An unimplemented name — a typo, or a Zig-ism k2 spells differently
(`@divTrunc`/`@rem` → `/`, `%`; `@divFloor`/`@mod` → `std.math.divFloor`/`mod`) —
is now `unknown builtin \`@…\`` at the call site, where it used to type-check as
`Deferred` and lower to a silent `undef` that ran and printed `<int>`. Closes a
"never miscompile" gap; pinned by a cli test.

## v0.38 — Recursive containers (leak analysis + first BST) ✅

The conservative leak pass now treats an allocation stored THROUGH a pointer
(`(*self).left = node`) as an ownership transfer — previously only `return`, a
paired free, and passing to a call counted, so a node-based structure tripped a
false "never freed". With that and the v0.37 `?*@This()` fix, the first recursive
container lands: **`std.BstNode(T)`**, a self-referential binary-search-tree node
(`insert`/`contains`/`count`/`height`/`min`/`max`/`freeChildren`). Genuine leaks
are still caught (verified). A clean generic *wrapper* (`root: ?*Node`) still
awaits memoizing a generic struct's nested type-const, so today's API is
node-based (the caller owns the root). Pinned by `conformance/10-stdlib/bst_node.k2`.

## v0.37 — Self-referential struct types ✅

A struct field may now point to its own type with `?*@This()` — the foundation for
linked lists, trees, and graphs — for both a top-level `struct` (also reachable by
its bare name, `?*Node`) and a generic node `Node(T)`. The type evaluator was
made two-phase on both the static (`eval_struct`) and the generic
(`eval_struct_comptime`) paths: a field-less shell is interned and exposed first,
the fields are evaluated against it, then patched in. Previously such a field
resolved to `?*deferred` and could not hold a real node pointer. Pinned by
`conformance/02-types/recursive_types.k2`; full suite green. A field naming a
*sibling* type const (`head: ?*Node`) is a separate decl-ordering gap, still open.

## v0.36 — Standard-library breadth ✅

A pure (or single-allocator) expansion of `std`, all VM-verified by the
`conformance/10-stdlib` corpus: **`std.str`** (byte-string search/trim/compare),
**`std.math`** (integer `isqrt`/`log2Int`/`popcount`/`clz`/`ctz`/`divFloor`/`mod`/
saturating add-sub), **`std.mem`** (`fill`/`reverse`/`swap`/`indexOfScalar`/`count`/
…), **`std.hex`** (encode/decode), and two allocator-owned containers,
**`std.RingBuffer(T)`** (circular FIFO) and **`std.BitSet`** (dense bit array).
Bringing up `std.str` flushed out a VM bug — slicing a string LITERAL
(`"x"[1..]`) produced a null-pointer slice — now fixed (native was already
correct). The container `init`s, like the other generics, are cleanly refused by
the native subset and run on the VM.

## Beyond 0.30

Full multi-platform self-hosting, an LLVM-grade optimizing middle-end,
Windows/macOS targets, an incremental/cached compilation cache, and a richer
package ecosystem — pursued once the language surface is stable at 1.0.

A few known limitations are deferred here deliberately, each a *clean refusal*
today rather than a miscompile:

- **`union(enum)` runtime values — landed in v0.31.** Tagged unions now store and
  retrieve their payload at run time on the VM (scalar AND aggregate payloads)
  and on the native x86-64 backend (scalar/`void` payloads). A union is a tag
  word plus a payload area sized to its largest variant; `switch` reads the tag
  and binds each arm's capture to the active payload. The one remaining gap is a
  *clean refusal*, never a miscompile: a variant with an **aggregate** (`struct`/
  array/slice) payload runs on the VM but is outside the native subset, so native
  refuses it. Bare untagged `union {…}` construction is likewise refused (no
  runtime discriminant). See the v0.31 milestone below.
- **Runtime `inline for`.** `inline for` unrolls in comptime-forced contexts
  (array lengths, `const` initializers), but a runtime-effectful `inline for`
  body — including iterating `@typeInfo(T).Struct.fields` to *print* each field's
  value at run time — is not yet lowered (it reaches a controlled VM panic-trap).
  `examples/comptime_reflection.k2` therefore demonstrates the comptime-reflection
  surface that *does* work (type inspection, `@sizeOf`, compile-time field
  validation, reflection-driven array sizing); full runtime reflection printing
  waits on this.
- **Wider native subset.** Generic containers with aggregate elements, fibers,
  the fs/net/time syscalls, and `union(enum)` variants with an **aggregate**
  payload run on the VM and are cleanly refused by the native backend;
  broadening native coverage is incremental future work.

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
