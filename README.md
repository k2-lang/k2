# k2

> **Total control over the machine, with zero waste.**

[![CI](https://github.com/k2-lang/k2/actions/workflows/ci.yml/badge.svg)](https://github.com/k2-lang/k2/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**k2** stands for *Kardashev Type II* — the point on the Kardashev scale where a
civilization harnesses the **total** energy output of its star. The name is a
promise about the language's relationship to the machine: complete, efficient,
deliberate control over every joule the CPU spends. Nothing is wasted, nothing
happens behind your back, and all the available power is yours to direct. The
lowercase, two-character name signals the ethos: a small surface area over
immense underlying power.

k2 is a **systems programming language** in the spirit of Zig, with a compiler
implemented in **Rust**. It compiles to **native machine code** with **no
garbage collector, no green-thread scheduler, and no language runtime** linked
into your binary. Zero-cost abstractions are a hard requirement: a k2
abstraction must compile to the same code a careful programmer would write by
hand.

What sets k2 apart from its inspiration is **no ambient authority**. Where Zig
makes allocation explicit but still exposes the OS through global namespaces, k2
extends "no hidden allocation" to *every* effect. I/O, the clock, randomness,
the environment, and the network are all capability values threaded explicitly
from a single `*System` handed to `main`. A function's signature is therefore an
honest, complete account of what it can do.

---

## Why k2 / Highlights

- **No hidden control flow.** Reading k2 code, you see exactly what runs. No
  exceptions, no destructors firing on scope exit, no operator overloading that
  dispatches to user code, no implicit coercions. The only non-linear control
  flow is the explicit set: `if`/`else`, `while`, `for`, `switch`, `try`,
  `catch`, `defer`, `errdefer`, `orelse`, `return`, `break`, `continue`, and
  `unreachable`.

- **No hidden allocation.** The language and standard library never allocate
  heap memory on your behalf. Any function that may allocate takes an explicit
  `Allocator` capability. If a value lives on the heap, the code that put it
  there is visible at the call site.

- **No ambient authority (capability-passing `*System`).** Every external effect
  — heap, I/O, clock, randomness, env, network — is reachable only through the
  `*System` handed to `main`. There are zero global side-effecting functions,
  which makes k2 code trivially **sandboxable** (hand a mock `System`),
  **deterministically testable** (hand a fake clock and seeded RNG), and
  **auditable** by signature alone.

- **`comptime` is the only metaprogramming.** No macro language, no template
  grammar, no separate generics syntax. Generics are ordinary functions that
  take `type` values at compile time and return types. Reflection is reified: the
  `@typeInfo` / `@Type` pair lets a program deconstruct a type into data and
  reconstruct types from data — all in plain k2 run by the compiler.

- **Errors are values.** Failures are carried in error unions (`E!T`), not a
  stack-unwinding channel. `try` propagates, `catch` handles, `errdefer` cleans
  up on the error path. An error value is a small integer tag, so error handling
  has the same cost model as any other branch.

- **Dual Cranelift/LLVM backend.** Cranelift delivers near-instant **Debug**
  builds with a full safety toolkit (bounds, overflow, and leak/use-after-free
  checks); LLVM delivers maximally optimized **ReleaseSafe**/**ReleaseFast**
  binaries from the same shared, monomorphized MIR.

- **Trivial cross-compilation.** The target triple is a build parameter. The
  toolchain bundles libc headers/stubs for common targets, and C interop is
  first-class via `extern` and an integrated C-translation path — no separate
  toolchain juggling.

---

## Hello, world

I/O is an explicit capability — there is no ambient stdout.

```k2
const std = @import("std");

/// Program entry point. `sys` is the root capability handle —
/// the sole source of authority in a k2 program.
pub fn main(sys: *System) !void {
    // No ambient stdout: we take the writer from the io capability.
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

A function that was never handed `sys.io` literally cannot print. The return
type `!void` is an error union: `main` may fail, and an unhandled error becomes
a nonzero process exit with the error name printed.

## Explicit allocation with `defer`

The heap is a capability too. Nothing reaches it without an `Allocator`, and
every allocation is paired with its release in the same scope.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // The heap is a capability; nothing allocates without it.
    const alloc = sys.heap;

    // Allocate a slice of 16 u32s. `try` propagates OutOfMemory.
    const buf: []u32 = try alloc.alloc(u32, 16);
    // Pair every allocation with its release in the same scope.
    defer alloc.free(buf);

    for (buf, 0..) |*slot, i| {
        slot.* = @intCast(i * i);
    }

    const out = sys.io.stdout();
    try out.print("buf[15] = {d}\n", .{buf[15]});
}
```

More worked examples — generic containers, error handling, comptime reflection,
and a `build.k2` build script — live in [`examples/`](examples/).

---

## Project status

> **Early / pre-alpha.** The front-end **skeleton** is implemented: a complete,
> spec-faithful **lexer** turns `.k2` source into a token stream, plus a small
> **AST** crate and a CLI driver. There is **no parser, type checker, comptime
> engine, or backend yet** — those phases are *specified* in [`docs/`](docs/) and
> tracked in [ROADMAP.md](ROADMAP.md). The k2 programs shown above and under
> `examples/` describe the **designed** language; they are not yet compilable.

What works today, end to end:

```sh
# Lex a real example and print its token stream.
$ cargo run -p k2c -- lex examples/hello.k2
# tokens for examples/hello.k2
# line col  kind             text
    10:1   KwConst          "const"
    10:7   Ident            "std"
    10:11  Eq               "="
    10:13  Builtin          "@import"
    10:20  LParen           "("
    10:21  StringLiteral    "\"std\""
    ...
```

---

## Build from source

k2's compiler is a Rust **Cargo virtual workspace** with three front-end crates.
It targets **stable Rust** and depends on the standard library *only* — there
are **zero third-party crates**, so the toolchain builds and runs fully offline.

```sh
# Clone and enter the repository.
git clone https://github.com/k2-lang/k2.git
cd k2

# Build the whole workspace (lexer, syntax, and the k2c driver).
cargo build

# Run the test suite (lexer unit tests + doctests across the crates).
cargo test

# Lex a file and print its token stream — the one working subcommand today.
cargo run -p k2c -- lex examples/hello.k2

# `tokenize` is the canonical name; `lex` is an alias. `-` reads stdin.
cargo run -p k2c -- tokenize examples/errors.k2
echo 'const x = 1;' | cargo run -p k2c -- lex -

# Usage and version.
cargo run -p k2c -- help
cargo run -p k2c -- version
```

The toolchain channel is pinned to `stable` (with `rustfmt` and `clippy`) in
[`rust-toolchain.toml`](rust-toolchain.toml).

### Workspace layout

| Crate | Path | Role |
| --- | --- | --- |
| `k2-lexer` | [`crates/k2-lexer/`](crates/k2-lexer/) | The lexer: `.k2` source → token stream. A faithful implementation of `docs/spec/01-lexical-structure.md`. |
| `k2-syntax` | [`crates/k2-syntax/`](crates/k2-syntax/) | AST node definitions and source spans, mirroring `docs/grammar.ebnf`. |
| `k2c` | [`crates/k2c/`](crates/k2c/) | The compiler driver (front-end CLI). Exposes the `tokenize`/`lex` subcommand today. |

---

## Documentation map

The design is fully specified ahead of the implementation. Start with the
philosophy, then the normative spec chapters.

- **[docs/philosophy.md](docs/philosophy.md)** — why k2 exists, the pillars, and
  the reasoning behind them.
- **[docs/compiler-architecture.md](docs/compiler-architecture.md)** — the
  pipeline (lex → HIR → sema → comptime → monomorphize → safety checks →
  Cranelift/LLVM) and the dual-backend strategy.
- **[docs/tooling.md](docs/tooling.md)** — the `k2` CLI, build modes, and
  developer workflow.
- **[docs/grammar.ebnf](docs/grammar.ebnf)** — the complete, self-consistent
  EBNF grammar for the whole language.

The language specification, chapter by chapter:

- [01 — Lexical Structure](docs/spec/01-lexical-structure.md)
- [02 — Types](docs/spec/02-types.md)
- [03 — Expressions and Statements](docs/spec/03-expressions-and-statements.md)
- [04 — Functions](docs/spec/04-functions.md)
- [05 — Memory and Allocators](docs/spec/05-memory-and-allocators.md)
- [06 — Error Handling](docs/spec/06-error-handling.md)
- [07 — Compile-Time Execution (`comptime`)](docs/spec/07-comptime.md)
- [08 — Modules and the Build System](docs/spec/08-modules-and-build.md)
- [09 — Concurrency](docs/spec/09-concurrency.md)
- [10 — The Standard Library](docs/spec/10-standard-library.md)

And a guided tour of the language through small, complete programs:

- **[examples/](examples/)** — `hello.k2`, `allocators.k2`, `generic_list.k2`,
  `errors.k2`, `comptime_reflection.k2`, and `build.k2`, with a walkthrough in
  [examples/README.md](examples/README.md).

---

## Roadmap

k2 is being built front-to-back from a locked language design. The current
milestone is **v0 — the front-end skeleton** (lexer done; parser next). After
that come the comptime engine, the Cranelift debug backend, the LLVM release
backend, the standard library, the package manager, an LSP/formatter, and the
road to **1.0**.

See **[ROADMAP.md](ROADMAP.md)** for the full milestone breakdown and an honest
account of current state.

---

## Contributing

Contributions are welcome — k2 is **design-first**: changes to language behavior
land in `docs/spec/` before they land in a crate. See
**[CONTRIBUTING.md](CONTRIBUTING.md)** for the repo layout, dev setup
(`cargo build` / `cargo test` / `cargo fmt` / `cargo clippy`), the proposal
process, commit/PR conventions, and good places to start.

---

## License

Licensed under either of

- **MIT** license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>), or
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
