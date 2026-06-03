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
  - `k2c` — the compiler driver, with a working `tokenize` / `lex` subcommand
    that streams tokens from a file or standard input.

- **Project infrastructure.** Continuous integration (`fmt` · `clippy` ·
  `build` · `test`, plus an examples smoke-test), contributor and security
  policies, dual MIT / Apache-2.0 licensing, and a development roadmap.

[Unreleased]: https://github.com/k2-lang/k2/commits/main
