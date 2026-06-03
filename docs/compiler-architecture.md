# Compiler Architecture

> Part of the **k2** language documentation.
> *k2: total control over the machine, with zero waste.*

This document describes how the **k2 compiler** is implemented in **Rust**: the
Cargo workspace and crate layout, the end-to-end pipeline from `.k2` source to a
native executable, the dual **Cranelift / LLVM** backend strategy, how the
**comptime** interpreter works, the plan for **incremental compilation**, and
why Rust is the right host language for the whole thing.

It is an architecture and design document, not an API reference. Names of
crates, passes, and intermediate representations are the canonical names used
throughout the toolchain; where an external Rust crate is named, it is a real,
load-bearing dependency, not a placeholder.

The compiler is named **`k2c`** (the driver binary). Everything below is in
service of two non-negotiable charter mandates that shape every design decision:

> **The speed mandate.** A k2 abstraction must compile to the same machine code
> a careful programmer would write by hand. Zero-cost is a hard requirement, not
> an aspiration.

> **No ambient authority.** The language model says effects are capabilities
> threaded from a `*System` root. The *compiler* mirrors this internally:
> compile-time code is sandboxed and cannot perform I/O or touch a runtime
> allocator. The capability discipline is enforced by the implementation, not
> merely documented.

---

## 1. The shape of the pipeline

Compilation is a sequence of total functions over typed intermediate
representations. Each stage consumes the output of the previous one and produces
a strictly more concrete artifact, and each stage lives in its own crate so the
boundaries are real module boundaries, not conventions.

```
 .k2 source
     │
     │  k2-lexer          UTF-8 bytes ─► token stream
     ▼
  tokens
     │  k2-parser         tokens ─► AST  (concrete syntax, lossless spans)
     ▼
   AST  (k2-syntax)
     │  k2-resolve        name resolution, module graph, @import wiring
     ▼
   HIR  (typed, names resolved)
     │  k2-resolve        bidirectional type inference, @TypeOf, capabilities
     ▼
   HIR  (fully typed)
     │  k2-comptime       sandboxed interpreter over typed HIR:
     ▼                    runs comptime code, instantiates generics,
   HIR  (comptime-folded) evaluates @typeInfo/@Type/@hasField/@compileError
     │  k2-ir             monomorphization ─► MIR (concrete, generics expanded)
     ▼
   MIR  (typed, monomorphic)
     │  k2-ir             safety-check insertion (Debug / ReleaseSafe)
     ▼
   MIR  (checks inserted)
     │  k2-codegen        backend selection:
     ▼                      Debug        ─► Cranelift
  object code               ReleaseSafe  ─► LLVM (checks kept)
     │                      ReleaseFast  ─► LLVM (checks stripped)
     │  k2c driver         cross-aware link with target system libraries
     ▼
  native executable / object
```

The arrows correspond one-to-one with the charter's stated compiler pipeline.
Three IRs do the heavy lifting:

- **AST** — the concrete syntax tree. Lossless: it retains every span and enough
  trivia to support good diagnostics and (later) formatting.
- **HIR** — the *high-level IR*. Desugared, names resolved, types attached.
  comptime evaluation operates here, because comptime is "ordinary k2 code run
  by the compiler" and therefore needs the same typed tree the runtime path
  uses.
- **MIR** — the *mid-level IR*. Fully monomorphized, all comptime values folded
  to constants, all generic functions specialized per distinct type argument,
  all safety checks made explicit. **Both backends consume MIR**, so Cranelift
  and LLVM share 100% of k2's semantics and differ only in optimization and
  check-stripping.

A single MIR feeding both backends is the architectural keystone of the dual
backend: there is exactly one definition of what a k2 program *means*, and two
ways to turn that meaning into machine code.

---

## 2. The Cargo workspace

The toolchain is a single Cargo **workspace**. Crates are split along the
pipeline boundaries so that each pass has a minimal, explicit dependency surface,
compiles in parallel, and can be tested in isolation.

```
k2/
├── Cargo.toml                 # [workspace] manifest, shared lints & profiles
├── crates/
│   ├── k2-span/               # SourceId, Span, source maps, line index
│   ├── k2-diag/               # Diagnostic, Severity, rendering (codespan)
│   ├── k2-intern/             # string/symbol interning, Symbol handles
│   ├── k2-lexer/              # bytes ─► tokens
│   ├── k2-syntax/             # AST node types + the green/red tree
│   ├── k2-parser/             # tokens ─► AST
│   ├── k2-resolve/            # HIR, name resolution, type inference
│   ├── k2-comptime/           # sandboxed comptime interpreter over HIR
│   ├── k2-ir/                 # MIR types, monomorphization, safety passes
│   ├── k2-codegen/            # backend traits + Cranelift & LLVM backends
│   │   ├── (cranelift module)
│   │   └── (llvm module, feature-gated)
│   ├── k2-link/               # cross-aware linker driver
│   ├── k2-build/             # build.k2 evaluation (k2 as the build system)
│   ├── k2-std/                # the std library, shipped as .k2 + intrinsics
│   └── k2c/                   # the driver binary (CLI, the `k2c` command)
└── ...
```

### Why these boundaries

- **`k2-span`, `k2-diag`, `k2-intern`** are the foundation crates every other
  crate depends on. Spans and interned symbols are passed by small `Copy`
  handles, never by owned strings, so threading source locations through ten
  passes costs nothing. Diagnostics are decoupled from the passes that emit them:
  a pass produces structured `Diagnostic` values; rendering to a terminal is a
  separate concern handled in `k2c`.
- **`k2-lexer` / `k2-parser` / `k2-syntax`** are deliberately separate. The AST
  type definitions (`k2-syntax`) are shared by the parser and by tools
  (formatter, language server) that want the tree without re-implementing
  parsing.
- **`k2-resolve`** owns both name resolution and type inference because in a
  language with first-class types and `@TypeOf` the two are entangled: resolving
  a name may require inferring a type, and inferring a type may require resolving
  a name inside a comptime expression.
- **`k2-comptime`** is its own crate precisely because it is the most dangerous
  one — it executes user code at compile time. Isolating it makes its sandbox
  boundary an actual API boundary (see §6).
- **`k2-codegen`** defines a backend-agnostic trait and contains two
  implementations. The LLVM implementation is **feature-gated** (`--features
  llvm`) so that a developer hacking on the front end can build the entire
  toolchain with only Cranelift and never link LLVM, keeping `cargo build` fast.
- **`k2-std`** is the standard library. It is mostly `.k2` source compiled like
  any other module, plus a small set of Rust-implemented **intrinsics** that the
  capability system (`System`, `Allocator`, the `io`/`clock`/`random` handles)
  binds to at the bottom. The compiler embeds the std `.k2` sources with
  `include_str!` so a k2 install is a single self-contained binary.

### Shared profiles

The workspace `Cargo.toml` pins build profiles so the compiler itself honors the
spirit of the speed mandate. The `release` profile sets `lto = "thin"` and
`codegen-units = 1`; a dedicated `dev-fast` profile keeps `opt-level = 1` for the
hot lexer/parser path even in debug builds, because a slow compiler undermines
the very feedback loop Cranelift exists to provide.

---

## 3. Front end: source → tokens → AST

### 3.1 Lexer (`k2-lexer`)

The lexer turns UTF-8 bytes into a flat `Vec<Token>`, where each `Token` is a
`(SyntaxKind, Span)` pair — the kind tag plus a byte range into the source. It is
hand-written rather than generated: k2's lexical grammar is small (the charter
keyword set, `@`-builtins, the postfix type sigils `?`, `!`, `*`, `[]`, and the
word operators `and` / `or` / `not`), and a hand-written scanner gives the best
error spans and the lowest latency.

Two details matter for the rest of the compiler:

- **`!` is purely an error-union sigil.** Because k2 uses the keywords `and`,
  `or`, `not` for boolean logic, the lexer never has to disambiguate `!` as
  "boolean not" versus "error union." `!T` and `E!T` are the only contexts, and
  that is a parser concern, not a lexer one.
- **Doc comments are tokens, not trivia.** `///` doc comments carry semantic
  weight (they attach to declarations), so they are real tokens; `//` line
  comments are trivia attached to the following token for round-tripping.

Symbols (identifiers, keywords) are interned through `k2-intern` at lex time, so
downstream comparisons are integer compares on `Symbol` handles.

### 3.2 Parser (`k2-parser`) and the AST (`k2-syntax`)

The parser is a hand-written **recursive-descent + Pratt** parser: recursive
descent for declarations and statements, Pratt (precedence-climbing) for the
expression grammar, which is the natural fit for an expression-oriented language
with postfix type modifiers and a clear operator precedence table.

The AST is a **lossless syntax tree** built on the **`rowan`** crate (the same
red/green tree design used by rust-analyzer). The green tree is immutable,
deduplicated, and cheap to clone; the red tree provides parent pointers and
absolute offsets on demand. Losslessness buys three things at once:

1. **Excellent diagnostics** — every node knows its exact span.
2. **A formatter for free** — `k2 fmt` walks the same tree.
3. **A language server later** — incremental reparsing of edited ranges.

The parser is **error-resilient**: on an unexpected token it inserts an error
node and continues, so a single typo yields one diagnostic and a still-analyzable
tree rather than a cascade. Parse errors are `Diagnostic` values from `k2-diag`.

Example — the parser accepts exactly the canonical surface syntax; nothing here
is novel, it is the same shape as every other k2 program:

```k2
const std = @import("std");

/// Program entry point. `sys` is the root capability handle.
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

The `pub fn main(sys: *System) !void` signature, the `@import` builtin, the
postfix `!void` error union, and the `try` operator are all recognized at this
stage and lowered to AST nodes; no meaning is assigned yet.

---

## 4. Middle end: resolution, typing, and the HIR

### 4.1 Lowering AST → HIR (`k2-resolve`)

The AST is desugared into the **HIR**, a tree designed for analysis rather than
faithful round-tripping. Lowering:

- expands a small amount of surface sugar into core forms (for example, `try e`
  becomes an explicit "evaluate `e`; if it is the error variant, `return` it"
  node — the charter is explicit that `try` is "the only sugar, and it expands
  to a visible early return," so it is desugared *visibly* and early);
- assigns every expression a stable `HirId`;
- keeps a side-table mapping each `HirId` back to its AST node and span, so a
  type error discovered three passes later still points at the right source.

### 4.2 The module graph and `@import`

`@import("std")` and `@import("./geometry.k2")` are resolved here. The resolver
builds a **module/namespace graph**: nodes are modules (one per `.k2` file plus
the synthetic `std` root), edges are `@import` dependencies. Cycles among value
declarations are an error; cycles among *type* declarations are permitted to the
extent the comptime engine can resolve them to a fixed point. Because each module
is a node with explicit edges, the graph is exactly what the incremental engine
(§7) keys on.

### 4.3 Name resolution

Name resolution binds every identifier to a definition: a local, a parameter, a
top-level `const`/`var`/`fn`, a struct field, an enum variant, or a builtin.
k2's rules are simple by design (charter pillar *one obvious way*), which keeps
the resolver simple: lexical scoping, `pub` controlling cross-module visibility,
and no overloading, so a name resolves to exactly one definition.

### 4.4 Type inference

Typing is **bidirectional** (charter: "Semantic analysis with bidirectional type
inference"). Two modes alternate:

- **check** mode pushes an expected type *down* into an expression (the
  annotation on a `var`, a function's declared return type, a parameter type);
- **synthesize** mode pulls a type *up* out of an expression when no expectation
  exists.

`@TypeOf(expr)` is resolved here by running synthesis on `expr` *without*
evaluating it at runtime — exactly its charter definition. Inferred-error unions
(`!T`, where the error set is collected from the function body) are computed by
unioning the error sets of every `try` and every `return error.X` in the body.

Capabilities participate in typing like any other value: `*System`,
`Allocator`, and the `io`/`clock`/`random`/`env`/`net` handles are ordinary
struct-of-function-pointer types. The type checker therefore enforces "no ambient
authority" structurally — a function whose signature does not mention a
capability simply has no value of that type in scope, so it *cannot* name the
operation. There is no special "purity" analysis; the absence is enforced by
ordinary scoping.

The result of the middle end is a **fully typed HIR**: every expression has a
type, every name has a definition, every error union is known. This is the tree
the comptime interpreter runs on.

---

## 5. comptime: an interpreter over the typed HIR

comptime is k2's *only* metaprogramming mechanism, so `k2-comptime` is where
generics, reflection, and constant folding all happen. The key architectural
decision: **comptime is an interpreter over the typed HIR, not a separate
language and not native-compiled code.**

### 5.1 Why an HIR interpreter and not codegen

A tempting alternative is to JIT comptime code to machine code and run it. k2
rejects this for three reasons that follow directly from the charter:

1. **Sandboxing.** comptime "cannot perform I/O or allocate from runtime
   allocators." An interpreter enforces this trivially: it simply does not
   implement opcodes for syscalls, and its only allocator is a compile-time
   arena owned by the compiler. A JIT would have to sandbox native code, which is
   far harder to make airtight.
2. **Determinism and portability.** comptime must produce identical results when
   cross-compiling. An interpreter over a portable IR is host-independent by
   construction.
3. **Termination and diagnostics.** comptime "is required to terminate," and
   `@compileError` must produce "a precise, source-located diagnostic." The
   interpreter carries a fuel/step budget and a call-stack of spans, so a
   runaway comptime loop or a rejected instantiation reports *where* in k2 source
   it happened.

### 5.2 What the interpreter evaluates

The interpreter is a tree-walking evaluator over typed HIR with a value model
covering comptime-known data: integers, floats, bools, arrays, structs, slices
into the compile-time arena, error values, and — crucially — **`type` values**.
A `type` is a first-class comptime value represented by a handle into the
compiler's type table. This is what makes generics "just functions."

It runs:

- **`comptime` expressions and parameters.** A parameter marked `comptime` must
  be supplied a value the interpreter can fully evaluate; if it cannot, that is a
  compile error at the call site.
- **Generics as functions.** `fn List(comptime T: type) type` is *called* by the
  interpreter with a concrete `T`, and it *returns a new struct type*, which is
  registered and cached. The growable list from the charter is the canonical
  case:

  ```k2
  /// A growable list. `List` is a function from a type to a type,
  /// evaluated at comptime — k2 has no separate generics syntax.
  pub fn List(comptime T: type) type {
      return struct {
          const Self = @This();

          items: []T,
          len: usize,
          alloc: Allocator,

          pub fn init(alloc: Allocator) Self {
              return Self{ .items = &.{}, .len = 0, .alloc = alloc };
          }

          pub fn push(self: *Self, value: T) !void {
              if (self.len == self.items.len) {
                  const new_cap = if (self.items.len == 0) 8 else self.items.len * 2;
                  self.items = try self.alloc.realloc(self.items, new_cap);
              }
              self.items[self.len] = value;
              self.len += 1;
          }
      };
  }
  ```

  When the program writes `List(u32)`, the interpreter evaluates the call,
  synthesizes the `struct` type with `T` bound to `u32`, and **memoizes it** keyed
  on the argument tuple `(u32)`. A second `List(u32)` anywhere in the program
  returns the *same* type; `List(u64)` produces a distinct one. This cache is the
  basis of monomorphization in §6.

- **Reflection builtins.** `@typeInfo(T)` materializes a tagged-union description
  of `T` as an ordinary comptime value the program can pattern-match;
  `@Type(info)` is its inverse, constructing a type from such a value;
  `@hasField`, `@field`, `@sizeOf`, `@alignOf`, `@TypeOf` are evaluated directly
  against the type table. The reflection printer from the charter exercises the
  whole path:

  ```k2
  /// Generate a field-by-field printer for ANY struct type at comptime.
  fn printFields(comptime T: type, out: anytype, value: T) !void {
      const info = @typeInfo(T);
      if (info != .Struct) {
          @compileError("printFields requires a struct, got " ++ @typeName(T));
      }
      inline for (info.Struct.fields) |field| {
          try out.print("{s} = {any}\n", .{ field.name, @field(value, field.name) });
      }
  }
  ```

  Here the interpreter evaluates `@typeInfo(T)`, runs the `if` over the result,
  unrolls the `inline for` over the comptime-known `fields` slice, and for each
  iteration resolves `@field(value, field.name)` — all before any runtime code
  exists. If `T` is not a struct, `@compileError` aborts with the spliced
  message and the call-site span.

- **`@compileError` / `@compileLog`.** `@compileError` raises a non-recoverable
  comptime diagnostic that the driver renders like any other error.
  `@compileLog` prints values during evaluation without halting — implemented as
  a side-channel the interpreter writes to and the driver flushes after the pass.

### 5.3 The comptime ↔ typing loop

comptime and type inference are not strictly sequential; they form a fixpoint.
Evaluating `List(u32)` *produces a type* that subsequent type-checking needs, and
type-checking a comptime argument may *require* evaluation. `k2-resolve` and
`k2-comptime` therefore cooperate through a shared **query interface** (§7):
"what is the type of this expression" and "what is the comptime value of this
expression" are mutually recursive queries with cycle detection, so a genuine
cyclic dependency is reported as such instead of looping forever.

---

## 6. Lowering to MIR: monomorphization and safety checks

### 6.1 Monomorphization (`k2-ir`)

After comptime evaluation the program still contains generic *call sites* (e.g.
`List(u32).init(sys.heap)`) referring to comptime-instantiated types.
Monomorphization walks the reachable call graph from `main` (and from `test`
declarations and exported `export` symbols) and emits one concrete MIR function
per `(function, comptime-arguments)` pair, reusing the memoization cache the
comptime interpreter already built. The output, **MIR**, has:

- no generic functions — every function is fully concrete;
- no comptime values — all folded to constants or baked into types;
- explicit types and layouts on every value, so `@sizeOf`/`@alignOf` results are
  literally the layout the backend will use;
- a simple, mostly-flat control-flow representation (basic blocks with explicit
  terminators) suited to straightforward lowering by *either* backend.

Because MIR is monomorphic and untyped-by-generics, the capability indirections
the language model describes (an `Allocator` is "a struct of function pointers
plus a context pointer") are visible as ordinary indirect calls — which means the
optimizer in the LLVM path can devirtualize and inline them when the concrete
allocator is known, collapsing the abstraction to hand-written-equivalent code.
That is the speed mandate cashing out at the IR level.

### 6.2 Safety-check insertion

For **Debug** and **ReleaseSafe** builds, a MIR pass inserts the safety checks
the charter requires: array-bounds checks on indexing, integer-overflow checks on
arithmetic, narrowing-cast checks for `@intCast`, and `unreachable`/`@panic`
guards. These are emitted *as ordinary MIR* — a comparison and a conditional
branch to a panic block — so the backends need no special knowledge of them; they
are just more basic blocks.

For **ReleaseFast**, this pass is skipped entirely. The checks do not exist in
the IR, so there is nothing for the optimizer to remove — violated assumptions
are undefined behavior, by design, for raw throughput.

### 6.3 comptime leak/escape heuristics

Also at the MIR/late-HIR boundary, `k2-ir` runs the **comptime leak/escape
analysis** the charter lists as a distinctive feature: simple, conservative
heuristics that flag obvious allocator misuse — a value allocated from a passed
`Allocator` and returned without an ownership transfer, or an allocation with no
paired `free`/`deinit` on a simple linear scope. These are *diagnostics*, not a
borrow checker: they catch the easy, high-value cases at compile time and defer
the rest to the runtime safety allocator. Anything the heuristic cannot prove it
stays silent about, so it never produces a false rejection of correct code.

---

## 7. Back end: the dual Cranelift / LLVM strategy

This is the defining implementation feature: **two backends behind one trait,
fed by one MIR**, selected by build mode.

`k2-codegen` defines a backend trait roughly of the shape "given a MIR module and
a target, produce object code." Two implementations satisfy it.

### 7.1 Cranelift — fast debug builds

Debug builds lower MIR to **Cranelift IR** using the **`cranelift-codegen`**,
**`cranelift-frontend`**, and **`cranelift-module`** / **`cranelift-object`**
crates. Cranelift is chosen for one reason: **near-instant compilation**. It does
little optimization, which is exactly right for the edit-compile-run loop where
the programmer wants the binary *now* and is running with full safety checks
anyway.

The Cranelift path is the default for `k2c build` (Debug mode) and the only
backend required to develop the compiler itself, which is why LLVM is
feature-gated. A contributor can `cargo build -p k2c` with no LLVM toolchain
present and have a fully working — if unoptimized — k2 compiler.

### 7.2 LLVM — optimized release builds

`ReleaseSafe` and `ReleaseFast` lower the *same* MIR to **LLVM IR** through the
**`inkwell`** crate (a safe Rust wrapper over **`llvm-sys`**, which binds the LLVM
C API). LLVM's optimization pipeline is where k2's zero-cost promise is realized:
capability indirect calls are devirtualized and inlined, generic code
specialized by monomorphization is optimized as ordinary concrete code, and the
abstraction overhead the source *looks* like it has disappears.

- **ReleaseSafe** keeps the safety checks inserted in §6.2 and asks LLVM to
  optimize *around* them — you get optimized code that still panics on a bounds
  violation.
- **ReleaseFast** runs the same LLVM pipeline over MIR that never had the checks
  inserted, producing the smallest, fastest possible code with undefined behavior
  on violated assumptions.

The choice of `inkwell`-over-`llvm-sys` (rather than hand-rolling FFI) keeps the
unsafe surface area minimal and type-checked by Rust, consistent with the "why
Rust" rationale below.

### 7.3 One IR, two backends — why it holds together

The two backends are interchangeable *because they consume identical MIR*. There
is no "Cranelift dialect" and "LLVM dialect" of k2 semantics; safety checks,
layouts, calling conventions, and capability lowering are all decided in
`k2-ir` before either backend sees the program. A program's *meaning* is fixed by
MIR; the backend only decides how hard to optimize and whether to keep the
checks. This is what lets the charter promise "the two paths share all semantics
and differ only in optimization and check-stripping."

### 7.4 Linking and cross-compilation (`k2-link`)

The target triple is a build parameter, not a global. The `k2-link` driver
selects the linker and the target's system libraries, drawing on the bundled
libc headers/stubs for common targets so cross-compilation needs no separate
toolchain juggling. C interop (`extern` declarations and the integrated
C-translation path) is wired in here so a k2 program that calls C links against
the right target objects regardless of host. Cross-compilation is first-class
precisely because the only host-specific step is this final link, and both
backends already emit object code for an arbitrary target triple.

---

## 8. The driver and `build.k2`

### 8.1 `k2c`

`k2c` is the CLI binary and the only crate that performs I/O. It parses
command-line arguments, reads source files, orchestrates the pipeline, renders
`Diagnostic` values to the terminal (via the **`codespan-reporting`** crate, with
**`termcolor`** for styling), and invokes the linker. Keeping all real-world
effects in the driver mirrors the language's own capability model: the *compiler*
has its ambient authority concentrated in one auditable place, just as a *k2
program* concentrates it in `*System`.

### 8.2 `build.k2` (`k2-build`)

A project's build is described in **`build.k2`**, written in ordinary k2 and
executed by the comptime engine — there is no second configuration language.
`k2-build` is a thin layer that: loads `build.k2` as a module, runs it through
the *same* `k2-comptime` interpreter used for in-program comptime, and reads back
the build description (targets, dependencies, cross triples, build modes) as
comptime values. The build system is k2 reflecting on itself, using exactly one
metaprogramming mechanism for both programs and their builds.

---

## 9. Incremental compilation

k2's fast-feedback promise (Cranelift) is doubled by an incremental front end.
The plan is a **query-based, demand-driven architecture** in the style of
rust-analyzer and the Rust compiler's own query system, built on the **`salsa`**
crate.

### 9.1 Queries instead of phases

Rather than running passes strictly front-to-back over the whole program, the
compiler is structured as a graph of **memoized queries**: "tokens of file *F*,"
"AST of *F*," "resolved scope of module *M*," "type of `HirId` *N*," "comptime
value of *N*," "MIR of function *G*." Each query is a pure function of its inputs;
`salsa` memoizes results and tracks the dependency graph between them.

### 9.2 Red-green invalidation

When a file changes, only the queries whose inputs actually changed are
recomputed (`salsa`'s red-green algorithm). Editing a function body invalidates
that function's MIR and its dependents, but not the typed HIR of an unrelated
module, and not the comptime instantiation cache for types it does not touch.
The lossless `rowan` tree supports **incremental reparsing**: an edit confined to
one function reparses that subtree and reuses the rest of the green tree.

### 9.3 What gets cached across builds

The intended on-disk artifacts are: per-module token and AST caches, the type
table, the comptime instantiation cache (memoized generic types and their MIR),
and per-function compiled objects. Because monomorphization keys on
`(function, comptime-arguments)`, an unchanged `List(u32)` is never
re-monomorphized or re-codegenned across builds. This is a *plan*: the query
skeleton is the v1 design point; full cross-process persistence is staged after
the single-process incremental engine lands.

### 9.4 Parallelism

Independent queries run in parallel. Parsing, resolution, and codegen of
independent modules are dispatched across a thread pool (**`rayon`**), which is
safe to do because each query is a pure function and `salsa` mediates shared
state. This is "fearless concurrency" applied to the compiler itself, and it is a
material part of why builds stay fast even before Cranelift contributes its
speed.

---

## 10. Why Rust is the right host

Every reason below is load-bearing, not incidental:

- **Memory safety in the compiler.** A compiler must never miscompile or crash on
  its *own* bugs. Rust's ownership and borrow checking eliminate the
  use-after-free, double-free, and data-race classes that have historically
  plagued systems-language compilers written in C and C++. A k2 user should never
  hit a segfault in `k2c`.
- **Enums and pattern matching map onto IRs.** AST, HIR, and MIR are
  algebraic data types; `@typeInfo` is itself a tagged union. Rust's `enum`s and
  exhaustive `match` express these directly, and exhaustiveness checking means
  adding a new IR node forces every pass to acknowledge it — the compiler's own
  type system keeps the many passes in sync.
- **A robust comptime interpreter.** The comptime engine executes
  untrusted-shaped compile-time code. Rust's `Result`-based error handling,
  strong typing, and absence of implicit nulls make the interpreter and its
  sandbox boundary refactor-safe; a bug there is a recoverable `Err`, not memory
  corruption.
- **Mature backend crates.** The dual-backend mandate is *achievable today*
  because both backends exist as Rust crates: `cranelift-codegen` for instant
  debug builds and `inkwell`/`llvm-sys` for optimized release builds. No bespoke
  codegen infrastructure is required.
- **Fearless concurrency.** `rayon` and `salsa` let the compiler parallelize and
  incrementalize across modules safely, directly serving the fast-build goal.
- **Cargo and the ecosystem.** `rowan`, `salsa`, `codespan-reporting`,
  `cranelift-*`, `inkwell`, `rayon`, and the interning/diagnostic crates are all
  one `Cargo.toml` line away, with reproducible builds and easy toolchain
  distribution. Cargo also cross-compiles `k2c` itself well, which supports the
  promise that k2 cross-compiles your programs.

There is a pleasing symmetry here: k2's thesis is "total, deliberate control with
zero waste," and Rust gives the *compiler* exactly that — control over memory and
concurrency with no garbage collector and no hidden cost — so the tool embodies
the value it ships.

---

## 11. Crate dependency summary

| Crate         | Consumes                | Produces                          | Key external crates                              |
| ------------- | ----------------------- | --------------------------------- | ------------------------------------------------ |
| `k2-span`     | —                       | `Span`, `SourceId`, line index    | —                                                |
| `k2-diag`     | `k2-span`               | `Diagnostic`, rendering           | `codespan-reporting`, `termcolor`                |
| `k2-intern`   | —                       | `Symbol` handles                  | (string interner)                                |
| `k2-lexer`    | source bytes            | token stream                      | —                                                |
| `k2-syntax`   | —                       | AST node types                    | `rowan`                                          |
| `k2-parser`   | tokens                  | AST (lossless tree)               | `rowan`                                          |
| `k2-resolve`  | AST                     | typed HIR, module graph           | `salsa` (query layer)                            |
| `k2-comptime` | typed HIR               | comptime values, instantiated types | (compile-time arena)                           |
| `k2-ir`       | comptime-folded HIR     | MIR (monomorphic, checks inserted) | —                                               |
| `k2-codegen`  | MIR                     | object code                       | `cranelift-codegen`/`-frontend`/`-object`, `inkwell` (`llvm-sys`) |
| `k2-link`     | object code             | executable / object               | (target-aware linker driver)                     |
| `k2-build`    | `build.k2`              | build plan                        | reuses `k2-comptime`                             |
| `k2-std`      | —                       | std `.k2` sources + intrinsics    | —                                                |
| `k2c`         | everything              | the `k2c` binary                  | `rayon`, `clap` (CLI), `codespan-reporting`      |

---

### See also

- **`docs/philosophy.md`** — the charter pillars (no hidden allocation, no
  ambient authority, comptime-only metaprogramming) that every architectural
  decision above is downstream of.
- **`docs/grammar.ebnf`** — the surface grammar the `k2-lexer` and `k2-parser`
  implement.
- **`docs/spec/05-memory-and-allocators.md`** — the `Allocator` capability and
  the Debug/ReleaseSafe/ReleaseFast safety-check model that §6.2 inserts and the
  backends in §7 keep or strip.
- **`docs/spec/06-error-handling.md`** — the error-as-values model that `try`
  desugaring (§4.1) and inferred-error-union inference (§4.4) implement.
