# Developer Tooling and the `k2c` CLI

> Part of the **k2** documentation.
> *k2: total control over the machine, with zero waste.*

> **Implementation status — design spec, lexer front-end only.** This document
> specifies the *designed* toolchain. The single driver binary is **`k2c`**
> (crate [`crates/k2c`](../crates/k2c/)), and today it implements exactly one
> working subcommand: **`k2c tokenize`** (alias `k2c lex`), which lexes a `.k2`
> file and prints its token stream. Everything else described below —
> `build`, `run`, `test`, `fmt`, `check`, `doc`, `lsp`, `pkg`, `init`,
> `targets` — is **specified but not yet implemented**. Do not expect `k2c run`
> or `k2c build` to work yet; until the backends land, study the example
> programs as designed k2, not as something the compiler can run. See
> [ROADMAP.md](../ROADMAP.md) for the per-subcommand implementation status, and
> `cargo run -p k2c -- help` for what the binary actually does today. The k2
> source shown throughout describes the designed language; it is not yet
> compilable.

This document specifies k2's developer tooling: the single **`k2c`** driver
(`build` / `run` / `test` / `fmt` / `check` and friends), the canonical
formatter **`k2c fmt`**, the language server **`k2c-lsp`**, the built-in **test
runner**, **documentation generation** from doc comments, and the **optimization
mode ladder** (`-O Debug` / `ReleaseSafe` / `ReleaseFast` / `ReleaseSmall`) that
selects between the fast Cranelift debug backend and the optimized LLVM release
backend.

Two charter pillars shape every decision below:

- **One obvious way, small surface.** There is one driver binary, one formatter
  with exactly one style and no options, one test mechanism (`test "name"
  { ... }` blocks), and one place a build is described (`build.k2`). The
  toolchain mirrors the language: a small surface you can hold in your head.
- **No ambient authority.** Tools are honest about effects. The compiler and its
  comptime engine never reach the network or the filesystem behind your back;
  fetching, running, and writing files are explicit, named steps. A test is
  handed mock capabilities, so the test runner is deterministic by construction.

This document assumes familiarity with `build.k2` and the package model (see
*Modules and the Build System*), the `*System` capability root, and the
Cranelift/LLVM backend strategy.

---

## Table of contents

1. [The `k2c` driver](#1-the-k2c-driver)
2. [Optimization modes: Debug, ReleaseSafe, ReleaseFast, ReleaseSmall](#2-optimization-modes-debug-releasesafe-releasefast-releasesmall)
3. [`k2c build` and `k2c run`](#3-k2c-build-and-k2c-run)
4. [`k2c fmt` — the canonical formatter](#4-k2c-fmt--the-canonical-formatter)
5. [`k2c check` — fast analysis without codegen](#5-k2c-check--fast-analysis-without-codegen)
6. [`k2c test` — the built-in test runner](#6-k2c-test--the-built-in-test-runner)
7. [`k2c doc` — documentation from doc comments](#7-k2c-doc--documentation-from-doc-comments)
8. [`k2c-lsp` — the language server](#8-k2c-lsp--the-language-server)
9. [`k2c pkg` and the rest of the driver](#9-k2c-pkg-and-the-rest-of-the-driver)
10. [How the tools share one compiler](#10-how-the-tools-share-one-compiler)
11. [Summary](#11-summary)

---

## 1. The `k2c` driver

The entire toolchain ships as **one binary**, `k2c`. There is no separate
formatter executable, no separate test binary, no separate doc generator to
install — every workflow is a subcommand of `k2c`, and they all share the same
front end (lexer, parser, resolver, comptime engine) and the same dual backend.
This is the same `k2c` driver the *Compiler Architecture* document describes: the
one crate that performs I/O and orchestrates the pipeline.

```text
k2c <subcommand> [arguments] [options]
```

The subcommands, grouped by what they do. The **Status** column records what is
implemented *today* versus what is specified for a later milestone; the
authoritative, milestone-by-milestone breakdown lives in
[ROADMAP.md](../ROADMAP.md).

| Subcommand | Purpose | Status |
|------------|---------|--------|
| `k2c tokenize` / `k2c lex` | Lex a `.k2` file (or stdin with `-`) and print its token stream. | **Implemented** — the one working code path today. |
| `k2c build` | Compile the project's artifacts via `build.k2`. | Specified — needs a backend (ROADMAP v0.7+). |
| `k2c run`   | Build the executable and run it, forwarding program arguments. | Specified — needs a backend (ROADMAP v0.7+). |
| `k2c test`  | Compile and execute every `test { ... }` block reachable from a test root. | Specified — needs a backend (ROADMAP v0.7+). |
| `k2c fmt`   | Rewrite source into the one canonical style. | Specified — needs the parser/AST (ROADMAP v0.13). |
| `k2c check` | Parse, resolve, type-check, and run comptime — **no codegen**. | Specified — needs the middle end (ROADMAP v0.3–v0.5). |
| `k2c doc`   | Generate API documentation from `///` doc comments. | Specified — needs resolve + comptime (ROADMAP v0.13). |
| `k2c lsp`   | Start the language server on stdio (also installed as `k2c-lsp`). | Specified (ROADMAP v0.13). |
| `k2c pkg`   | Manage content-addressed dependencies (`fetch` / `update` / `hash`). | Specified (ROADMAP v0.12). |
| `k2c init`  | Scaffold a new project (`build.k2`, `k2.pkg`, `src/main.k2`). | Specified (ROADMAP v0.12). |
| `k2c targets` | List the target triples the bundled toolchain can cross-compile to. | Specified (ROADMAP v0.8). |
| `k2c version` | Print the compiler and bundled-`std` versions. | **Implemented**. |
| `k2c help` | Print usage. | **Implemented**. |

Until the backends land, the only commands that do real work are `k2c tokenize`
(alias `k2c lex`), `k2c help`, and `k2c version`; you can run them straight from
the workspace with `cargo run -p k2c -- <subcommand>`. The remainder of this
document specifies the *intended* behavior of the full driver.

A handful of options are **global** — they mean the same thing for every
subcommand that compiles code:

| Option | Meaning |
|--------|---------|
| `-O <mode>` | Optimization mode: `Debug`, `ReleaseSafe`, `ReleaseFast`, `ReleaseSmall`. |
| `-Doptimize=<mode>` | The same selection, in the `build.k2` option spelling. |
| `-Dtarget=<triple>` | Cross-compilation target triple (`arch-os-abi`). |
| `-D<name>[=<value>]` | Set a custom `build.k2` option (e.g. `-Dwith-tls=true`). |
| `--color <when>` | `auto` (default), `always`, or `never` for diagnostic color. |
| `--json-diagnostics` | Emit structured diagnostics for editor/CI consumption. |
| `-j <n>` | Cap the number of parallel compilation jobs. |

`-O Debug` and `-Doptimize=Debug` are exactly equivalent; `-O` is the short,
muscle-memory spelling, and `-Doptimize=` is the literal `build.k2` option that
`b.standardOptimize()` reads. The driver normalizes the former into the latter
before evaluating `build.k2`, so there is a single source of truth — consistent
with **One obvious way**.

Every code-producing subcommand evaluates the project's `build.k2` with the
comptime engine (see *Modules and the Build System*), then walks the requested
step graph. `k2c fmt`, `k2c check`, and `k2c lsp` operate directly on source and
do not require a `build.k2`, so they work on a bare `.k2` file with no project
around it.

---

## 2. Optimization modes: Debug, ReleaseSafe, ReleaseFast, ReleaseSmall

The charter's backend strategy is **dual**: Cranelift for near-instant debug
builds, LLVM for maximally optimized release builds, with a shared monomorphized
MIR feeding both so the two paths differ only in optimization and check
stripping. That strategy surfaces on the command line as **one option with four
values** — the optimization mode. (Both backends are specified, not yet built;
see ROADMAP v0.7–v0.8.)

| Mode | Backend | Safety checks | Optimization | Use it for |
|------|---------|---------------|--------------|------------|
| **`Debug`** (default) | Cranelift | All on | None | Edit–compile–run inner loop; fastest builds. |
| **`ReleaseSafe`** | LLVM | All on | Full | Production where a checked panic beats undefined behavior. |
| **`ReleaseFast`** | LLVM | **Stripped** | Full, speed-tuned | Maximum throughput; you accept UB on violated assumptions. |
| **`ReleaseSmall`** | LLVM | **Stripped** | Full, size-tuned | Smallest binary: embedded, Wasm, distribution size. |

What the modes share and where they diverge:

- **`Debug`** uses **Cranelift** for the fastest possible compile, paired with
  the full safety toolkit: bounds checks, integer-overflow checks, narrowing-cast
  checks (`@intCast`), `unreachable` traps, and the leak/double-free/use-after-free
  detecting `GeneralPurposeAllocator`. This is the default because the tight
  feedback loop is the day-to-day reality of writing code.
- **`ReleaseSafe`** lowers the **same MIR** through **LLVM** for full
  optimization while *keeping* every safety check. A violated assumption produces
  a deterministic, located `@panic` rather than undefined behavior — the right
  default for shipping software where correctness outranks the last few percent.
- **`ReleaseFast`** lowers through LLVM and **strips the safety checks** in the
  safety-check-insertion pass. The checks do not exist in the emitted code, so an
  out-of-bounds access or overflow is *undefined behavior*, not a panic. This is
  the "harness every joule" mode: zero-cost abstractions collapse to
  hand-written-equivalent machine code with no guard rails.
- **`ReleaseSmall`** is `ReleaseFast`'s sibling: same LLVM backend, same
  check-stripping, but the optimizer is tuned for **code size** rather than raw
  speed (favoring smaller codegen, more aggressive size folding, and minimal
  unrolling). It targets binary footprint — freestanding firmware, `wasm32`
  bundles, and distributed executables where bytes cost.

Because all four modes consume the **same typed, monomorphized MIR**, they share
100% of k2's semantics. A program's *meaning* never depends on the mode; only
whether a violated safety assumption **panics** (Debug, ReleaseSafe) or is
**undefined** (ReleaseFast, ReleaseSmall), and how the backend optimizes.

```sh
k2c run                         # Debug: Cranelift, all checks, instant build
k2c build -O ReleaseSafe        # LLVM, optimized, checks kept
k2c build -O ReleaseFast        # LLVM, optimized for speed, checks stripped
k2c build -O ReleaseSmall       # LLVM, optimized for size, checks stripped
```

Inside `build.k2`, the mode arrives as an `OptimizeMode` value through
`b.standardOptimize()`, and it is comptime-known in your program if you surface
it as a build option, so you can branch on it with dead-branch elimination:

```k2
// build.k2 (excerpt)
const build = @import("build");

pub fn build(b: *Build) void {
    const target = b.standardTarget();
    const optimize = b.standardOptimize();   // reads -O / -Doptimize=

    const exe = b.addExecutable(.{
        .name = "app",
        .root_source = b.path("src/main.k2"),
        .target = target,
        .optimize = optimize,
    });
    // Make the mode visible to program code as a comptime constant.
    exe.addOption(@TypeOf(optimize), "mode", optimize);
    b.installArtifact(exe);
}
```

```k2
// src/main.k2 (excerpt) — the mode is a comptime value, so the
// extra-logging branch vanishes entirely from release builds.
const opts = @import("build_options");

fn logVerbose(out: anytype, comptime fmt: []const u8, args: anytype) !void {
    if (opts.mode == .Debug) {
        try out.print(fmt, args);
    }
    // In ReleaseFast/ReleaseSmall this whole call lowers to nothing.
}
```

---

## 3. `k2c build` and `k2c run`

`k2c build` and `k2c run` are specified in detail in *Modules and the Build
System*; this section covers only their tooling-facing surface. Both depend on a
working backend and are not yet implemented (ROADMAP v0.7 lands `k2c run` /
`k2c build` against the Cranelift debug backend).

`k2c build` evaluates `build.k2`, then compiles the requested step graph with the
backend selected by `-O`. With no step name it runs the synthesized `install`
step, emitting every artifact passed to `b.installArtifact(...)` into the output
directory (`zig-out`-style layout: `<out>/bin`, `<out>/lib`). A named step runs
that step and its transitive dependencies.

```sh
k2c build                                 # default install step, Debug
k2c build run -- --input data.json        # run a build-script "run" step
k2c build -O ReleaseFast -Dtarget=aarch64-linux-musl   # optimized cross-build
k2c build --json-diagnostics              # machine-readable diagnostics for CI
```

`k2c run` is the shorthand for "build the executable and run it." For a
single-executable project the driver infers the artifact; otherwise it runs the
`run` step the build script registered. Arguments after `--` are forwarded to the
program, where they arrive through the **`sys.env`** capability — never through
an ambient `argv`:

```sh
k2c run -- --verbose --threads 8
```

```k2
// src/main.k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // Arguments are read through the env capability; a function not handed
    // `sys.env` cannot see them. Allocation is explicit and paired with free.
    const args = try sys.env.args(sys.heap);
    defer sys.heap.free(args);

    try out.print("received {d} argument(s)\n", .{args.len});
}
```

---

## 4. `k2c fmt` — the canonical formatter

`k2c fmt` is the **one true formatter**: it rewrites k2 source into a single
canonical style. There are **no formatting options** — no indent-width flag, no
brace-style flag, no line-length knob. The style is the style. This is **One
obvious way** taken to its conclusion: formatting is never a debate, never a
per-project config file, never a diff that is "just whitespace from someone
else's editor." (`k2c fmt` needs the parser and the lossless AST; it is specified
for ROADMAP v0.13 and not yet implemented.)

### 4.1 What the canonical style is

The style matches every snippet in this documentation and in the charter's
canonical snippets exactly:

- **4-space indentation**, never tabs.
- **K&R braces**: the opening `{` stays on the line that introduces the block;
  the closing `}` aligns with that line's start.
- One statement per line, each terminated by `;`.
- A single space around binary operators (`a + b`, `x == y`, `a and b`), after
  commas, and after the keywords `if`/`while`/`for`/`switch` before their `(`.
- **No space** between a function name and its `(`, between a builtin's `@name`
  and its `(`, or inside the postfix type sigils (`?T`, `*T`, `[]const u8`,
  `E!T`).
- Trailing commas in multi-line aggregate literals and parameter lists, so adding
  a field is a one-line diff.
- A trailing newline at end of file; no trailing whitespace on any line.
- Doc comments (`///`) immediately precede the declaration they document; line
  comments (`//`) are kept as written but re-indented to their block.

The formatter is **idempotent** (`k2c fmt` on already-formatted source is a
no-op and produces byte-identical output) and **semantics-preserving** (it only
moves whitespace and normalizes comma/brace placement; it never reorders
declarations, renames anything, or changes which code runs).

### 4.2 Usage

```sh
k2c fmt                 # format every .k2 file under the current directory, in place
k2c fmt src/main.k2     # format a single file in place
k2c fmt src/            # format a directory tree in place
k2c fmt --stdin         # read source on stdin, write formatted source to stdout
k2c fmt --check         # format nothing; exit nonzero if any file is unformatted
```

`--check` is the CI gate. It prints the paths that *would* change and exits with
a nonzero status if any file is not already canonical, without modifying the
working tree:

```sh
# In CI: fail the build if anyone forgot to run the formatter.
k2c fmt --check
```

`--stdin` is what editors call on save (the language server also exposes
formatting as an LSP request, see §8), so "format on save" needs no plugin
beyond the standard editor integration.

### 4.3 Before and after

`k2c fmt` takes input like this:

```k2
const std=@import( "std" );
pub fn  main(sys:*System)!void{
const out=sys.io.stdout();
try out.print("Hello, k2!\n",.{});}
```

and rewrites it to the canonical form, identical to the charter's hello-world:

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

Because there is exactly one target form, the formatter is also the answer to
"what does idiomatic k2 look like?": run `k2c fmt`, and the output *is* the style
guide.

---

## 5. `k2c check` — fast analysis without codegen

`k2c check` runs the entire front end — lex, parse, import resolution, name
resolution, bidirectional type inference, and **comptime evaluation** — and then
**stops before codegen**. It reports every error the compiler would report
(syntax errors, type errors, capability/visibility errors, failed `@compileError`
diagnostics, comptime leak/escape warnings) but emits no object code, so it is
dramatically faster than a full build. (`k2c check` comes online as the middle
end does, across ROADMAP v0.3–v0.5; today only the lexing portion of this
pipeline exists.)

```sh
k2c check                   # type-check the whole project
k2c check src/parser.k2     # check a single file (and its imports)
k2c check --json-diagnostics
```

This is the command an editor's "problems" panel and a pre-commit hook want: it
is the authoritative answer to "does this compile?" without paying for Cranelift
or LLVM. Because it runs the comptime engine, `k2c check` catches the same
generic-instantiation and reflection errors a build would — a `List(SomeType)`
that fails `@compileError`, a `@typeInfo` switch that is non-exhaustive, an
obvious allocate-and-return-without-ownership leak — all surfaced as
**compile-time diagnostics**, located precisely in source, exactly as the
charter's comptime leak/escape analysis promises.

---

## 6. `k2c test` — the built-in test runner

Testing is **part of the language**, not a library bolted on. `test` is a
keyword, and a `test` block is a named, capability-receiving function the
compiler collects and runs. (Running tests needs a backend; `k2c test` is
specified and arrives with the Cranelift debug backend in ROADMAP v0.7.)

```k2
test "name of the behavior under test" {
    // ordinary k2; use `try` to fail the test on an error.
}
```

### 6.1 `test` blocks

Test blocks live alongside the code they exercise. They are compiled **only** in
test builds: in `k2c build` and `k2c run`, a `test` block is skipped entirely and
contributes **zero bytes** to the shipped binary.

```k2
const std = @import("std");

/// The unit under test. It takes its allocator as a capability, so a test
/// can hand it a leak-detecting testing allocator.
fn doubleAll(alloc: Allocator, src: []const u32) ![]u32 {
    const dst: []u32 = try alloc.alloc(u32, src.len);
    for (src, 0..) |value, i| {
        dst[i] = value * 2;
    }
    return dst;
}

test "doubleAll doubles every element" {
    // The test runner hands each test a testing allocator. `defer` frees the
    // result; if a test leaks, the testing allocator fails the test.
    const alloc = std.testing.allocator;

    const out = try doubleAll(alloc, &.{ 1, 2, 3 });
    defer alloc.free(out);

    try std.testing.expectEqual(@as(usize, 3), out.len);
    try std.testing.expectEqual(@as(u32, 6), out[2]);
}

test "doubleAll of empty slice is empty" {
    const out = try doubleAll(std.testing.allocator, &.{});
    defer std.testing.allocator.free(out);
    try std.testing.expectEqual(@as(usize, 0), out.len);
}
```

The assertion helpers live in `std.testing` and return error unions, so a failed
assertion is an ordinary error value propagated with `try` — there is no separate
assertion control-flow channel, consistent with **Errors are values**:

| Helper | Fails the test unless… |
|--------|------------------------|
| `std.testing.expect(cond)` | `cond` is `true`. |
| `std.testing.expectEqual(expected, actual)` | the two are equal. |
| `std.testing.expectError(err, expr)` | `expr` evaluates to error `err`. |
| `std.testing.expectEqualSlices(T, a, b)` | the slices are element-wise equal. |

```k2
const std = @import("std");

const ParseError = error{ Empty, NotANumber };

fn parseU32(text: []const u8) ParseError!u32 {
    if (text.len == 0) return ParseError.Empty;
    var value: u32 = 0;
    for (text) |ch| {
        if (ch < '0' or ch > '9') return ParseError.NotANumber;
        value = value * 10 + @as(u32, ch - '0');
    }
    return value;
}

test "parseU32 rejects empty input" {
    try std.testing.expectError(ParseError.Empty, parseU32(""));
}

test "parseU32 reads a decimal number" {
    try std.testing.expectEqual(@as(u32, 42), try parseU32("42"));
}
```

### 6.2 Tests receive mock capabilities

The payoff of **No ambient authority** is that tests are **deterministic by
construction**. Effectful code takes its effects as capabilities, so a test
hands it *mock* capabilities instead of the real machine — no monkey-patching, no
global override, no injection framework:

- a **testing allocator** that detects leaks, double-frees, and use-after-free
  and fails the test if any occur;
- a **fake `Clock`** whose `monotonic()` returns a scripted sequence;
- a **`Random`** seeded to a fixed value for reproducible randomized tests;
- an in-memory **`Fs`** and an in-memory **`Io`** sink to capture output.

```k2
const std = @import("std");

/// Effectful code: it formats into whatever writer it is handed.
fn greet(out: anytype, name: []const u8) !void {
    try out.print("hello, {s}!\n", .{name});
}

test "greet writes the expected bytes" {
    var buf: [64]u8 = undefined;
    // An in-memory writer stands in for stdout — no real I/O in the test.
    var sink = std.io.fixedBufferStream(&buf);
    try greet(sink.writer(), "k2");

    // The output is now plain data to assert on.
    try std.testing.expect(std.mem.eql(u8, sink.getWritten(), "hello, k2!\n"));
}
```

A test that exercises time-, randomness-, or filesystem-dependent code does the
same thing: construct a fake capability, hand it in, assert on the result. The
test runner itself is just the harness that constructs these mocks and threads
them into each `test` block.

### 6.3 Running tests

`k2c test` builds the project's test artifacts (the test roots wired up in
`build.k2`) and runs every `test` block reachable from each test root's
`root_source`. The runner reports per-test pass/fail, the failing assertion's
location, leak reports from the testing allocator, and a final summary.

```sh
k2c test                          # run all tests, Debug (Cranelift, all checks)
k2c test --filter "parse"         # only tests whose name contains "parse"
k2c test -O ReleaseSafe           # run the suite optimized, checks still on
k2c test -Dtarget=wasm32-wasi     # cross-compile tests and run them under WASI
```

Example output:

```text
Test [3/5] doubleAll doubles every element ... OK
Test [4/5] parseU32 rejects empty input ..... OK
Test [5/5] parseU32 reads a decimal number .. FAIL
    src/parse.k2:31:5: expected 42, found 4
    test "parseU32 reads a decimal number"

4 passed; 1 failed; 0 skipped (Debug, x86_64-linux-gnu)
```

Tests run under **Debug** by default, so the leak-detecting allocator and all
safety checks are active — the configuration most likely to catch bugs. Running
the same suite under `-O ReleaseSafe` exercises the LLVM-optimized code path with
checks intact, a valuable second gate before shipping.

---

## 7. `k2c doc` — documentation from doc comments

`k2c doc` generates API documentation from **`///` doc comments**, the same doc
comments the language already specifies. Documentation is therefore written in
the source, next to what it describes, and stays correct because it lives where
the code lives. (`k2c doc` reuses the resolver and comptime engine, so it lands
once those exist; it is specified for ROADMAP v0.13.)

```sh
k2c doc                       # generate docs for the project's public API
k2c doc --out docs/api        # choose the output directory
k2c doc --format html         # html (default) or markdown
k2c doc src/root.k2           # document a single module
```

### 7.1 What gets documented

`k2c doc` walks the same module graph the compiler builds and emits an entry for
every **`pub`** declaration reachable from the documented root — `pub const`,
`pub var`, `pub fn`, and the `pub` members of `pub` types. Non-`pub`
declarations are private to their file and are **omitted**, so the generated docs
are exactly the package's public surface, no more. The `///` text attached to
each declaration becomes its description; the **signature is read from the actual
code**, never restated by hand, so it can never drift.

```k2
/// A growable list of `T`. `List` is a function from a type to a type,
/// evaluated at comptime — k2 has no separate generics syntax.
///
/// Ownership: `init` allocates nothing; `push` grows through the stored
/// allocator; the caller must call `deinit` to free the backing storage.
pub fn List(comptime T: type) type {
    return struct {
        const Self = @This();

        items: []T,
        len: usize,
        alloc: Allocator,

        /// Create an empty list that will allocate through `alloc`.
        pub fn init(alloc: Allocator) Self {
            return Self{ .items = &.{}, .len = 0, .alloc = alloc };
        }

        /// Free the backing storage. Call exactly once.
        pub fn deinit(self: *Self) void {
            self.alloc.free(self.items);
        }

        /// Append `value`, growing capacity if needed.
        /// Returns `error{OutOfMemory}` if the allocator cannot grow.
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

From this, `k2c doc` produces an entry for `List` with its doc comment, and nested
entries for `init`, `deinit`, and `push` with their real signatures and error
unions. The generated page makes the **capability and error contract visible**:
a reader sees `init` takes an `Allocator`, that `push` returns `!void`, and the
prose explains the ownership rule.

### 7.2 How it works (and why it is honest)

`k2c doc` is built on the same reflection the language exposes to programs. It runs
the front end and then uses the structured type information (`@typeInfo`-style
descriptions of structs, enums, unions, error sets, and function signatures) to
render each declaration. Because it reuses the compiler's resolver and comptime
engine, the documentation reflects the code **as the compiler actually sees it** —
generic functions are documented as the type-to-type functions they are, error
sets are listed exactly, and a doc comment whose described function no longer
exists is a build-time mismatch, not stale prose nobody noticed.

Like every other tool, `k2c doc` performs no hidden effects: it reads the source
you point it at and writes to the output directory you name, and nothing else.

---

## 8. `k2c-lsp` — the language server

`k2c-lsp` is the editor-facing **language server**, speaking the standard Language
Server Protocol over stdio. It is the same binary as the driver — `k2c lsp` starts
the server, and the toolchain also installs a `k2c-lsp` alias so editors that
expect a dedicated executable find one. Because it is built on the **same front
end** as `k2c build`/`k2c check`, the diagnostics, types, and definitions an editor
shows are *identical* to what the compiler reports; there is no second, divergent
analyzer to drift out of sync. (The server reuses the front-end crates and is
specified for ROADMAP v0.13; it is not yet implemented.)

### 8.1 Capabilities

`k2c-lsp` provides the standard editor features, each backed by a real compiler
pass rather than a heuristic:

| Feature | Backed by |
|---------|-----------|
| **Diagnostics** (errors, warnings) | The resolver and comptime engine — same errors as `k2c check`, pushed live as you type. |
| **Hover** (type and doc) | `@TypeOf`-style inference plus the `///` doc comment of the symbol under the cursor. |
| **Go-to-definition / references** | The name-resolution graph, across files and `@import` boundaries. |
| **Completion** | Members of the resolved type or namespace at the cursor, including capability methods reachable from `sys`. |
| **Formatting** | The exact `k2c fmt` engine — format-on-save with no extra config. |
| **Rename** | The resolver's symbol graph (rename a `pub fn`, update every reference). |
| **Signature help** | The callee's real parameter list, including `comptime` parameters. |
| **Inlay hints** | Inferred types for `const`/`var` bindings and `|capture|` payloads. |

### 8.2 Capability-aware assistance

`k2c-lsp` understands the **capability model**, which makes its hints unusually
precise. Because a function's signature is an honest account of what it can do,
the server can tell you, on hover, that a function taking only an `Allocator`
*cannot* read the clock or touch the network — and completion after `sys.` offers
exactly the capability handles the charter defines (`sys.heap`, `sys.io`,
`sys.clock`, `sys.random`, `sys.env`, `sys.net`) and their methods. There is no
guessing about ambient globals, because there are none.

### 8.3 Incremental and fast

The server is designed for the same tight loop Cranelift serves: it reparses only
the edited ranges, re-resolves only the affected module subgraph, and reuses the
comptime memoization cache so unchanged generic instantiations are not
re-evaluated. The result is live diagnostics that keep pace with typing, drawn
from the authoritative compiler — never a watered-down approximation.

```sh
k2c lsp                 # start the server on stdio (what editor extensions launch)
k2c lsp --stdio         # explicit transport (default)
```

Editor extensions configure their k2 plugin to launch `k2c lsp` (or `k2c-lsp`); no
per-project server configuration is required, because the project's `build.k2`
already describes module wiring and the server reads it.

---

## 9. `k2c pkg` and the rest of the driver

`k2c pkg` manages the project's content-addressed dependencies, specified fully in
*Modules and the Build System*. In tooling terms, the key property is that
**fetching is the only network step, and it is explicit** (the package manager is
specified for ROADMAP v0.12 and not yet implemented):

```sh
k2c pkg fetch        # download + verify locked deps missing from the cache
k2c pkg update       # re-resolve manifests, rewrite k2.lock to newest allowed
k2c pkg hash <url>   # fetch an archive and print its content hash (for k2.pkg)
```

`k2c build`/`k2c run`/`k2c test` invoke `fetch` implicitly when a locked dependency
is absent from the cache, but **never** `update` — moving a dependency is always
a deliberate act. Evaluating a manifest (`k2.pkg`) or a build script (`build.k2`)
is pure comptime and reaches neither the network nor the filesystem; the comptime
sandbox stays honest, which is **No ambient authority** applied to the build
engine itself.

The remaining driver subcommands are conveniences:

```sh
k2c init                 # scaffold build.k2, k2.pkg, and src/main.k2
k2c init --lib           # scaffold a library package instead of an executable
k2c targets              # list cross-compilation triples the toolchain bundles
k2c version              # print compiler and bundled-std versions
```

`k2c init` produces a minimal, already-canonical project so the first `k2c run`
works immediately:

```k2
// src/main.k2 — generated by `k2c init`.
const std = @import("std");

/// Program entry point. `sys` is the root capability handle — the sole
/// source of authority in a k2 program.
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

---

## 10. How the tools share one compiler

The defining property of k2's tooling is that **every tool is the same
compiler**, invoked at a different stopping point. The compiler pipeline runs:

```text
source ──▶ lex/parse ──▶ resolve + infer ──▶ comptime ──▶ monomorphize (MIR)
                                                              │
                          ┌───────────────────┬──────────────┴───────────┐
                          ▼                   ▼                          ▼
                       Cranelift            LLVM                     (stop early)
                       (Debug)        (ReleaseSafe/Fast/Small)     fmt / check / lsp / doc
```

- **`k2c fmt`** stops after **parse**: it needs only the AST (the same syntax-tree
  types the parser produces), which is why it works on a bare file with no project
  and never needs to type-check.
- **`k2c check`** and **`k2c-lsp`** stop after **comptime evaluation**: full
  semantic analysis, no codegen. They report exactly the errors a build would,
  including comptime leak/escape diagnostics and `@compileError`s.
- **`k2c doc`** stops after **resolve + comptime** and renders the structured type
  information of `pub` declarations — the same reflection a program could request
  with `@typeInfo`.
- **`k2c build` / `k2c run` / `k2c test`** run the full pipeline through
  **monomorphization** and into a backend chosen by `-O`: **Cranelift** for
  `Debug`, **LLVM** for the three `Release*` modes, both consuming the **same
  MIR**.

Today the pipeline is implemented only as far as the **lexer**, so `k2c tokenize`
is the single subcommand that runs end to end; every later stopping point above is
specified and waiting on its pass (see [ROADMAP.md](../ROADMAP.md)). The
architecture, however, is exactly this — one front end, stopped at different
points — and it is the architecture the *Compiler Architecture* document
describes for the `k2c` driver.

This is **One obvious way, small surface** at the tooling layer: there is no
separate parser in the formatter, no separate analyzer in the language server, no
separate type-checker in the doc generator. Fix a bug in the resolver and every
tool gets it. Add a language feature and the formatter, the LSP, the doc
generator, and both backends understand it because they are literally the same
front end. The dual backend is the *only* fork in the pipeline, and it is a
deliberate one: fast feedback on the left, harnessed-star throughput on the right.

---

## 11. Summary

- The toolchain is **one binary**, `k2c`, with subcommands `tokenize`/`lex`
  (the only one implemented today), plus the specified `build`, `run`, `test`,
  `fmt`, `check`, `doc`, `lsp`, `pkg`, `init`, `targets`, and `version`. The
  per-subcommand implementation status is tracked in
  [ROADMAP.md](../ROADMAP.md); *Compiler Architecture* describes the same `k2c`
  driver.
- The Cranelift/LLVM backend split surfaces as **one option, four values**:
  `-O Debug` (Cranelift, all checks, fastest build), `ReleaseSafe` (LLVM,
  optimized, checks kept), `ReleaseFast` (LLVM, optimized for speed, checks
  stripped), `ReleaseSmall` (LLVM, optimized for size, checks stripped). `-O` and
  `-Doptimize=` are the same selection; all four modes share one MIR and one
  meaning.
- **`k2c fmt`** is the canonical formatter with **exactly one style and no
  options**; `--check` is the CI gate, `--stdin` is format-on-save. Its output
  *is* the style guide.
- **`k2c check`** runs the whole front end through comptime with **no codegen** —
  the fast, authoritative "does this compile?", including comptime leak/escape
  diagnostics.
- **`k2c test`** runs the language-level `test "name" { ... }` blocks, which are
  compiled only in test builds, receive **mock capabilities** (testing allocator,
  fake clock, seeded RNG, in-memory I/O), and assert via `std.testing` helpers
  that return error unions. Tests are deterministic by construction.
- **`k2c doc`** generates documentation from `///` doc comments for the **`pub`**
  surface, reading signatures from the real code via the compiler's reflection so
  docs never drift.
- **`k2c-lsp`** (`k2c lsp`) is a capability-aware language server built on the
  **same front end** as the compiler, so its diagnostics, types, and definitions
  match `k2c check` exactly.
- **Every tool is the same compiler** stopped at a different point. One front end,
  one comptime engine, two backends — small surface, no waste, nothing happening
  behind your back. Today that compiler runs as far as the lexer; the rest is
  specified and on the roadmap.

---

### See also

- **ROADMAP.md:** the honest, milestone-by-milestone implementation status — which
  of the subcommands above are built, in progress, or specified, and in what order
  they land.
- **Modules and the Build System:** `build.k2`, the `*Build` capability,
  `b.standardOptimize()` / `b.standardTarget()`, the `k2c build`/`k2c run`/`k2c test`
  step graph, and content-addressed packages (`k2.pkg` / `k2.lock`).
- **The Philosophy of k2:** *One obvious way, small surface* and *No ambient
  authority*, the pillars the tooling embodies.
- **Compiler Architecture:** the `k2c` driver, the shared AST (`k2-syntax`) the
  formatter and language server reuse, the comptime sandbox behind
  `k2c check`/`k2c doc`, and the dual Cranelift/LLVM backend the optimization modes
  select.
- **Standard Library:** `std.testing` assertions and mock capabilities, `std.io`
  in-memory writers, and the `Allocator`/`Clock`/`Random`/`Fs` capability types
  tests substitute.
