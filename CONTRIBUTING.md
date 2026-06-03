# Contributing to k2

> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

Thanks for your interest in k2! The language is in **early / pre-alpha** (see
[ROADMAP.md](ROADMAP.md)): the design is locked and fully specified under
[`docs/`](docs/), and the implementation has reached a working **lexer**. That
means there is a lot of high-leverage, well-scoped work to do — and the design
is stable enough that you can contribute without fear of building on sand.

k2 is **design-first**. The single most important rule in this document:

> **Behavior is defined in `docs/spec/` before it is written in a crate.** If
> your change alters what the language *does*, the spec change is the proposal,
> and the code is the follow-through.

---

## Repository layout

k2's compiler is a Rust **Cargo virtual workspace**. It targets **stable Rust**,
uses **no nightly features**, and depends on the **standard library only** —
there are zero third-party crates, so the toolchain builds and tests fully
offline. Please keep it that way (see [Dependencies](#dependencies)).

### Crates (`crates/`)

| Crate | Path | What it owns |
| --- | --- | --- |
| `k2-lexer` | [`crates/k2-lexer/`](crates/k2-lexer/) | The lexer: `.k2` source → token stream. Co-normative with `docs/spec/01-lexical-structure.md` and the lexical terminals in `docs/grammar.ebnf`. Pure function from bytes to tokens; no I/O, no panics on bad input. |
| `k2-syntax` | [`crates/k2-syntax/`](crates/k2-syntax/) | AST node definitions (`Item`, `Stmt`, `Expr`, `Param`, `SourceFile`) and source `Span`s, mirroring `docs/grammar.ebnf`. Depends only on `k2-lexer` (for the `Token` type). |
| `k2c` | [`crates/k2c/`](crates/k2c/) | The compiler driver / front-end CLI. Hand-rolled argument parsing; today it exposes the `tokenize`/`lex` subcommand. |

### Docs (`docs/`)

| File | Contents |
| --- | --- |
| [`docs/philosophy.md`](docs/philosophy.md) | The pillars and the reasoning behind them. Read this first. |
| [`docs/grammar.ebnf`](docs/grammar.ebnf) | The complete EBNF grammar for the whole language. |
| [`docs/compiler-architecture.md`](docs/compiler-architecture.md) | The pipeline and the dual Cranelift/LLVM backend strategy. |
| [`docs/tooling.md`](docs/tooling.md) | The `k2` CLI, build modes, and workflow. |
| [`docs/spec/01`–`10`](docs/spec/) | The normative language specification, chapter by chapter. |

### Examples (`examples/`)

[`examples/`](examples/) holds small, complete k2 programs (`hello.k2`,
`allocators.k2`, `generic_list.k2`, `errors.k2`, `comptime_reflection.k2`,
`build.k2`) plus a walkthrough in [`examples/README.md`](examples/README.md).
These describe the *designed* language; they are not yet compilable, but every
`.k2` file **must lex cleanly** with `k2c lex` and stay consistent with the
locked charter (same keywords, same `@builtins`, same declaration forms).

---

## Development setup

You need a **stable Rust toolchain**. The channel and components are pinned in
[`rust-toolchain.toml`](rust-toolchain.toml), so `rustup` will fetch the right
ones automatically:

```sh
# Install Rust if you don't have it.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# From the repo root — rustup honors rust-toolchain.toml (stable + rustfmt + clippy).
cargo build
cargo test
```

### Everyday commands

```sh
# Build the whole workspace.
cargo build

# Run all tests: lexer unit tests + the doctests in k2-lexer / k2-syntax.
cargo test

# Format. CI requires this to be clean.
cargo fmt --all

# Check formatting without modifying files (what CI runs).
cargo fmt --all -- --check

# Lint. Treat warnings as errors, as CI does.
cargo clippy --all-targets -- -D warnings

# Exercise the driver on a real example.
cargo run -p k2c -- lex examples/hello.k2
echo 'const x = 1;' | cargo run -p k2c -- lex -
```

Before opening a PR, make sure all four of these pass locally:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test
```

---

## The design-first process

Because the language is locked to a charter and a spec, changes fall into two
clearly different buckets. Knowing which one you are in saves everyone time.

### 1. Implementation work (most contributions)

You are turning *already-specified* behavior into code: writing the parser,
fixing a lexer edge case the spec already describes, improving a diagnostic,
adding tests, or cleaning up Rust. **No proposal needed** — open a PR that cites
the relevant spec section and matches it.

If you find the **code and the spec disagree**, that is a bug. Open an issue.
Unless the spec is self-contradictory, **the spec wins** and the code is fixed
to match it.

### 2. Language changes (propose to the spec first)

If your change would alter what k2 *means* — a new keyword, different syntax, a
new builtin, a semantic tweak — do **not** start in a crate. Instead:

1. **Open an issue** describing the problem and your proposed change. Reference
   the affected spec chapter(s) and explain how it fits the pillars
   (`docs/philosophy.md`): no hidden control flow, no hidden allocation, no
   ambient authority, comptime-only metaprogramming, one obvious way.
2. **Discuss it.** k2 deliberately favors a *small surface*. Where two features
   would overlap, k2 ships one. "It would be convenient" is not enough; the bar
   is that the language is better *and* still holds the whole of itself in a
   reader's head.
3. **Propose the spec edit** (a PR to `docs/spec/` and, if grammar changes,
   `docs/grammar.ebnf`). The spec PR is the design review.
4. **Then implement** it against the agreed spec, in a follow-up PR.

The charter's locked elements — the keyword set, the `@builtin` set, the
`pub fn main(sys: *System) !void` entry point, the capability model, `!` as the
error-union constructor with `and`/`or`/`not` as keywords — are **not up for
casual change**. Proposals touching them need a strong, pillar-aligned argument.

---

## Coding conventions

- **Match the surrounding style.** The existing crates are heavily and
  thoughtfully documented; keep that bar. Public items get `///` doc comments;
  non-obvious logic gets a comment explaining *why*, with a spec reference where
  one applies (e.g. `// spec §7.2`).
- **`rustfmt` is law.** Do not hand-format around it.
- **`clippy` clean** with `-D warnings`. If you must allow a lint, scope the
  `#[allow(...)]` as narrowly as possible and say why.
- **Tests live next to the code** in `#[cfg(test)] mod tests`. New lexer/AST
  behavior needs tests; bug fixes should come with a regression test that fails
  before the fix.
- **Error recovery, not panics.** The front-end never panics on malformed
  *input* — a lexical error becomes an `Error` token; a parse error becomes a
  spanned diagnostic with recovery. Reserve `panic!`/`unwrap` for genuine
  internal invariants.
- **Keep `.k2` examples valid.** Any `.k2` file you add or edit must lex without
  `Error` tokens and use only charter vocabulary.

### Dependencies

**Do not add third-party crates** without prior discussion in an issue. The
no-dependencies, stable-only, offline-buildable property is a deliberate feature
of the toolchain, not an accident. (Cranelift and LLVM bindings will arrive with
the backend milestones; until then, the front-end stays std-only.)

---

## Commit and PR conventions

### Commits

- One logical change per commit; keep history readable.
- **Imperative, present-tense subject** under ~72 characters, optionally
  prefixed with the area: `lexer: handle CRLF in multiline strings`,
  `syntax: add Param span helper`, `docs: clarify §6 errdefer ordering`.
- Explain the *why* in the body when it is not obvious from the diff.

### Pull requests

- **Keep PRs focused.** One concern per PR; split unrelated changes.
- **In the description**, link the issue (and, for language changes, the spec
  PR), summarize the change, and note how you tested it.
- **Confirm the local checklist passed**: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`. CI
  ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs the same steps.
- For language-behavior changes, the matching `docs/spec/` (and grammar) updates
  must be in the same PR or an already-merged predecessor — code and spec never
  diverge on `main`.

---

## Where to start

Good first areas, roughly easiest to hardest:

- **Docs and examples.** Fix typos, tighten wording, add clarifying comments to
  spec chapters or `examples/*.k2`. Low risk, high value — and a great way to
  learn the language.
- **Lexer hardening.** Add tests for tricky inputs (unusual numeric literals,
  multiline strings, escaped identifiers, recovery after an error token), or
  improve an error message. The lexer is small and well-tested; it is the
  friendliest crate to get oriented in.
- **`k2c` driver ergonomics.** Better usage text, clearer diagnostics, or new
  read-only subcommands that build on the existing token stream.
- **The parser (v0.2).** The current headline milestone — a recursive-descent
  parser producing the `k2-syntax` AST per `docs/grammar.ebnf`. Larger, but the
  AST and grammar already exist to build against. Coordinate on the tracking
  issue first so work doesn't collide.

Look for issues labeled **`good first issue`** and **`help wanted`**. If you are
unsure whether an idea fits, **open an issue and ask before writing code** —
especially for anything touching the language design.

---

## Licensing

Be respectful and constructive; assume good faith.

By contributing, you agree that your contributions are dual-licensed under
**MIT OR Apache-2.0**, matching the project (see the crate manifests and the
README's License section). Unless you state otherwise, any contribution you
submit for inclusion is licensed as above with no additional terms.

Welcome aboard — let's direct every joule.
