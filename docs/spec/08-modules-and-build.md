# 08 — Modules and the Build System

> Part of the **k2** language specification.
> *k2: total control over the machine, with zero waste.*

This chapter specifies how k2 programs are organized into **source files**,
how those files become **modules** through `@import`, how **visibility**
(`pub`) draws the boundary of a module's public surface, how files are grouped
into **packages**, and how an entire project is described, compiled, tested,
and cross-compiled by a **`build.k2`** file written in k2 itself.

Two charter pillars govern everything below:

- **comptime is the only metaprogramming.** A `build.k2` is not a second
  configuration language. It is ordinary k2 executed by the compiler's
  comptime engine. The build graph is a value you construct with the same
  syntax, types, and tooling as the program being built.
- **One obvious way, small surface.** There is exactly one mechanism for
  pulling in code (`@import`), one keyword for exporting (`pub`), one file that
  describes a build (`build.k2`), and one manifest format for dependencies.
  No Makefile dialect, no YAML, no second grammar.

This chapter assumes familiarity with declarations (`const`/`var`), function
syntax, `pub` (§04), and the postfix-modifier type grammar.

---

## 1. Source files are implicit structs

A `.k2` source file *is* a `struct` type. Every top-level declaration in a file
is a member of an anonymous, comptime-known struct, and `@import` evaluates to a
value of that struct type — a namespace.

This is the single most important fact about k2 modules: there is no separate
"module" construct layered on top of the type system. A file and a `struct`
are the same kind of thing, so everything you already know about structs —
`pub` members, nested types, `const` bindings, functions — applies unchanged to
files.

Consider a file `math.k2`:

```k2
// math.k2 — the whole file is an implicit `struct { ... }`.

/// Exported: visible to anyone who imports this file.
pub const pi: f64 = 3.141592653589793;

/// Exported function.
pub fn square(x: f64) f64 {
    return x * x;
}

/// File-private: not part of the namespace `@import` returns.
fn clampNonNegative(x: f64) f64 {
    return if (x < 0.0) 0.0 else x;
}

/// A nested type is just a `pub const` bound to a `struct`.
pub const Vec2 = struct {
    x: f64,
    y: f64,

    pub fn lengthSquared(self: Vec2) f64 {
        return square(self.x) + square(self.y);
    }
};
```

Because a file is a struct, the following two spellings are conceptually
identical. The file `math.k2` above is the same as if you had written, in
another file:

```k2
const math = struct {
    pub const pi: f64 = 3.141592653589793;
    pub fn square(x: f64) f64 { return x * x; }
    fn clampNonNegative(x: f64) f64 { return if (x < 0.0) 0.0 else x; }
    pub const Vec2 = struct {
        x: f64,
        y: f64,
        pub fn lengthSquared(self: Vec2) f64 { return square(self.x) + square(self.y); }
    };
};
```

The file form is the idiomatic one; you almost never write a top-level
`const Name = struct { ... }` whose only purpose is to be a namespace, because a
file already is one.

### 1.1 The implicit file struct has no fields, only declarations

The file struct's members are all `const`/`var`/`fn`/`pub` *declarations*. A
file cannot declare instance *fields* (like `x: f64,`) at the top level —
fields belong to a named `struct`, `enum`, or `union` written explicitly inside
the file. This keeps the "a file is a namespace" model clean: importing a file
gives you a namespace of declarations, never a struct you instantiate.

---

## 2. `@import` — loading modules

`@import` is the **only** mechanism for pulling one piece of k2 code into
another. It is a comptime builtin (`@`-sigil, per §01) that takes a single
compile-time-known string and returns the imported file's top-level namespace
as a comptime value:

> **`@import`** — Load a module (e.g. `@import("std")`) or another k2 source
> file by path, returning its top-level namespace as a comptime value.

There are two argument forms, distinguished by syntax, never by guesswork:

```k2
const std = @import("std");          // a named module (resolved via the manifest)
const math = @import("math.k2");     // a path relative to THIS file
```

### 2.1 Path imports

If the string ends in `.k2` or contains a `/`, it is a **path import**,
resolved relative to the directory of the importing file:

```k2
const math = @import("math.k2");              // sibling file
const geom = @import("geometry/shapes.k2");   // subdirectory
const root = @import("../app.k2");            // parent directory
```

Path imports are how a package's own files refer to one another. The path is
purely lexical and resolved at comptime during the import-resolution pass of the
pipeline (§01 of the compiler pipeline: *"resolve imports (`@import`) and build
the module/namespace graph"*). A file may not import a path that escapes its
package root; cross-package references go through named imports (§2.2).

### 2.2 Named imports

If the string is a bare identifier-like name (no `.k2`, no `/`), it is a
**named import** of a *package*: `std`, or any dependency declared in the
project manifest (§7). The mapping from name to package is established by the
build, not hard-coded into the compiler — `std` is the one name the toolchain
always provides.

```k2
const std = @import("std");          // always available
const json = @import("json");        // a dependency named in build.k2 / manifest
```

A named import resolves to the package's **root module** — the single file the
package nominates as its public entry point (its `root_source`, §6.2). Importing
a package never reaches into its internal files; you see exactly the namespace
its root file exports.

### 2.3 `@import` is comptime and acyclic-at-the-value-level

`@import` returns a comptime value, so its result is a first-class type-or-namespace
you can bind, pass to generic functions, or reflect on with `@typeInfo`:

```k2
const std = @import("std");
const math = @import("math.k2");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const v = math.Vec2{ .x = 3.0, .y = 4.0 };
    try out.print("|v|^2 = {d}, pi = {d}\n", .{ v.lengthSquared(), math.pi });
}
```

Imports may refer to each other (file A imports B which imports A) as long as
the *values* they need from one another are resolvable without infinite
recursion — the same termination rule that governs all comptime evaluation.
Mutually recursive *types* across files are fine; a comptime computation that
genuinely requires its own not-yet-finished result is a compile error, reported
with a precise, source-located diagnostic.

---

## 3. Visibility: `pub`

A declaration is **private to its file** unless prefixed with `pub`. `pub`
adds the declaration to the namespace that `@import` returns. This single rule
covers files, structs, and packages, because they are the same kind of thing.

```k2
pub const VERSION: u32 = 3;          // visible to importers
pub fn parse(text: []const u8) !Value { ... }   // visible to importers

const internal_table = [_]u8{ 1, 2, 3 };         // file-private
fn helperInternal(x: i32) i32 { return x; }      // file-private
```

The rules, stated precisely:

- **Top-level declarations.** A top-level `const`/`var`/`fn` is reachable
  through `@import` **iff** it is `pub`. Non-`pub` declarations are usable
  *within the file* but absent from the imported namespace.
- **Struct/enum/union members.** A `pub` member (field or method) is visible to
  code that has a value or namespace of that type; a non-`pub` member is visible
  only inside the file that declares the type. Fields of a `struct` are `pub` or
  private per-field, exactly like top-level declarations.
- **Packages.** A package exposes only what its **root module** marks `pub`.
  Files reachable only through internal path imports are private to the package
  no matter what they themselves mark `pub` — `pub` controls visibility *within
  the import that reaches the file*, and the outside world never reaches an
  internal file directly.

There is no `private` keyword, no `protected`, and no `internal` modifier:
absence of `pub` is the only "not exported" state, which keeps the surface
small and the default safe (private).

### 3.1 Re-exporting

Because an import is just a value, you re-export by binding it to a `pub const`.
This is the idiomatic way a package's root module assembles its public API out
of internal files:

```k2
// root.k2 — the package's public entry point.

const shapes = @import("internal/shapes.k2");   // internal file, NOT re-exported

pub const Circle = shapes.Circle;   // re-export one type from an internal file
pub const Square = shapes.Square;

pub const util = @import("util.k2"); // re-export an entire sub-namespace
```

Importers of this package see `Circle`, `Square`, and `util`, but cannot reach
`internal/shapes.k2` directly. You curate the public surface deliberately,
declaration by declaration.

---

## 4. Modules and packages

The two organizing units are **modules** and **packages**.

- A **module** is a single `.k2` file together with the namespace it exports —
  i.e. one implicit file struct. "Module" and "imported file" are synonyms.
- A **package** is a versioned, independently distributable collection of
  modules with **one nominated root module**, a name, and (for non-trivial
  packages) a manifest. A package is the unit of dependency, versioning, and
  named import.

The relationship:

```
package "json"
├── manifest: k2.pkg          (name, version, dependencies)
├── root module: src/root.k2  (the only thing importers see)
└── internal modules:
    ├── src/lexer.k2          (reached only via path import from root)
    └── src/value.k2
```

`std` is a package like any other; the toolchain simply guarantees it is always
present and always resolvable by the name `"std"`. There is nothing magic about
its shape — it is modules and `pub` declarations all the way down.

A program (an *executable* package) and a *library* package differ only in what
the build produces from them: an executable package has a root module with the
entry point `pub fn main(sys: *System) !void`; a library package has a root
module that exports types and functions and has no `main`.

---

## 5. The build system is k2: `build.k2`

A project is built by a **`build.k2`** file in its root directory, written in
ordinary k2 and executed by the compiler's comptime engine. This is a charter
distinctive feature:

> **build.k2 — the build system is k2 itself.** A project's build is described
> in a `build.k2` file written in ordinary k2, executed by the compiler's
> comptime engine. No second configuration language, no YAML, no Makefile
> dialect: targets, dependencies, cross-compilation triples, and build modes are
> expressed with the same language, types, and tooling as the program being
> built.

### 5.1 Shape of a `build.k2`

A `build.k2` exposes a single entry point:

```k2
pub fn build(b: *Build) void { ... }
```

`b: *Build` is a **build capability** — the build-time analogue of `*System`.
Where `*System` is the program's authority over the running machine,
`*Build` is the build script's authority over the build graph: it is how you
declare executables, libraries, and tests, read user-selected target and
optimization options, wire up dependencies, and register run/test steps. As with
`*System`, there is no ambient build state; everything flows through `b`.

The `Build` type and its helper types (`Target`, `OptimizeMode`, `Step`,
`Module`, `Dependency`) live in the build module:

```k2
const build = @import("build");      // the build-system standard module
```

`build(b)` is run by the comptime engine when you invoke `k2 build`, `k2 run`,
or `k2 test`. It does not compile anything itself; it **describes** a graph of
build steps. The compiler then executes that graph with the real backends
(Cranelift for Debug, LLVM for Release; see §05 of the charter's backend
strategy).

### 5.2 Build modes and the target

Two values are surfaced from the command line into `build(b)` and threaded into
every artifact:

- **`OptimizeMode`** — one of `Debug`, `ReleaseSafe`, `ReleaseFast`, matching the
  charter's build-mode ladder. `Debug` uses Cranelift with the full safety
  toolkit; `ReleaseSafe` uses LLVM but keeps safety checks; `ReleaseFast` uses
  LLVM and strips checks.
- **`Target`** — a resolved target triple (architecture, OS, ABI). The default is
  the host; any other triple makes the build a cross-compile (§8).

You obtain both through `b`, with helpers that apply the user's `-Doptimize=...`
and `-Dtarget=...` choices and supply sensible defaults:

```k2
pub fn build(b: *Build) void {
    const target = b.standardTarget();        // honors -Dtarget=..., defaults to host
    const optimize = b.standardOptimize();    // honors -Doptimize=..., defaults to Debug
    // ...
}
```

---

## 6. A realistic `build.k2`

The following is a complete, idiomatic build for a project that produces a
library, a CLI executable that links it, a unit-test step, and an
integration-test step — with target and optimization mode chosen on the command
line, one external dependency, and a `run` step.

```k2
// build.k2 — built and run by the k2 comptime engine.
const build = @import("build");

/// The single build entry point. `b` is the build capability:
/// no ambient build state, everything flows through `b`.
pub fn build(b: *Build) void {
    // --- User-selectable options (filled from the command line) --------------
    // `standardTarget` honors -Dtarget=<triple>, defaulting to the host.
    const target = b.standardTarget();
    // `standardOptimize` honors -Doptimize={Debug,ReleaseSafe,ReleaseFast}.
    const optimize = b.standardOptimize();

    // A custom boolean option, queryable as -Dwith-tls=true|false.
    const with_tls = b.option(bool, "with-tls", "Build with TLS support") orelse false;

    // --- External dependencies (resolved from the manifest, §7) --------------
    // `dependency` looks up a package declared in k2.pkg by name and pins it to
    // the content-addressed version recorded in the lockfile.
    const json_dep = b.dependency("json", .{
        .target = target,
        .optimize = optimize,
    });
    const json_mod = json_dep.module("json");   // the dep's exported module

    // --- The library artifact ------------------------------------------------
    const lib = b.addLibrary(.{
        .name = "geo",
        .root_source = b.path("src/root.k2"),
        .target = target,
        .optimize = optimize,
    });
    // Compile-time configuration is passed as an ordinary build option, read
    // back in code via @import("build_options").
    lib.addOption(bool, "with_tls", with_tls);
    // Make `geo` available to importers as the module name "geo".
    b.installArtifact(lib);

    // --- The executable artifact ---------------------------------------------
    const exe = b.addExecutable(.{
        .name = "geocli",
        .root_source = b.path("src/main.k2"),
        .target = target,
        .optimize = optimize,
    });
    // Wire modules into the executable's import namespace. Inside src/main.k2,
    // `@import("geo")` and `@import("json")` now resolve to these modules.
    exe.addModule("geo", lib.module());
    exe.addModule("json", json_mod);
    b.installArtifact(exe);

    // --- A `run` step: `k2 build run` invokes the freshly built exe ----------
    const run_exe = b.addRunArtifact(exe);
    run_exe.passForwardedArgs();              // forward args after `--` to the program
    const run_step = b.step("run", "Build and run geocli");
    run_step.dependOn(&run_exe.step);

    // --- Unit tests: every `test { ... }` block in the library ---------------
    const unit_tests = b.addTest(.{
        .name = "unit",
        .root_source = b.path("src/root.k2"),
        .target = target,
        .optimize = optimize,
    });
    const run_unit = b.addRunArtifact(unit_tests);

    // --- Integration tests: a separate test root that imports the library ----
    const integ_tests = b.addTest(.{
        .name = "integration",
        .root_source = b.path("tests/integration.k2"),
        .target = target,
        .optimize = optimize,
    });
    integ_tests.addModule("geo", lib.module());
    const run_integ = b.addRunArtifact(integ_tests);

    // Aggregate both into a single `test` step: `k2 build test`.
    const test_step = b.step("test", "Run all tests");
    test_step.dependOn(&run_unit.step);
    test_step.dependOn(&run_integ.step);
}
```

### 6.1 What the graph means

`build(b)` constructs a directed acyclic graph of `Step` values. Nothing is
compiled while `build` runs; the function is pure description, evaluated by the
comptime interpreter (which, like all comptime, **cannot perform I/O or allocate
from a runtime allocator** — it only builds the graph). Afterward the compiler
walks the requested step's dependencies, compiles each artifact with the
appropriate backend, and runs `run`/`test` steps.

Steps the user can name on the command line are the ones you register with
`b.step("name", "description")`. The toolchain also synthesizes default steps:
the top-level `install` step (run by a bare `k2 build`) gathers everything passed
to `b.installArtifact(...)`.

### 6.2 `root_source` and module wiring

Each artifact names exactly one `root_source` — the module that *is* the
artifact's public surface and entry point. Other files are reached from the root
via path imports (`@import("...k2")`). Cross-package imports (`@import("geo")`,
`@import("json")`) resolve to whatever modules you wired in with
`exe.addModule(name, module)`. The build script is therefore the single place
that maps import names to packages; code never hard-codes a dependency's
location.

### 6.3 Build options are comptime values in your code

`lib.addOption(bool, "with_tls", with_tls)` makes a comptime constant available
to the compiled code through a synthetic module:

```k2
const opts = @import("build_options");

pub fn connect(...) !void {
    if (opts.with_tls) {
        // ... TLS path, compiled in only when the build enabled it.
    } else {
        // ... plaintext path.
    }
}
```

Because `opts.with_tls` is comptime-known, the dead branch is eliminated entirely
in the monomorphized MIR — zero runtime cost for a compile-time configuration
switch, consistent with the zero-cost-abstraction mandate.

---

## 7. The package manager: manifest + content-addressed dependencies

A package that has dependencies (or that is itself published) carries a
**manifest** named `k2.pkg`, written — like everything else — in k2. It is
evaluated at comptime and yields a single value describing the package's
identity and its dependency edges.

### 7.1 The manifest: `k2.pkg`

```k2
// k2.pkg — the package manifest, evaluated at comptime.
const build = @import("build");

pub const package = build.Package{
    .name = "geocli",
    .version = "1.4.0",

    // The module(s) this package exports to dependents. A library package
    // names its public root here; an application can omit `exports`.
    .root_source = "src/root.k2",

    // Dependency edges. Each entry pins a NAME to a SOURCE plus an integrity
    // hash. The name on the left is what `@import("...")` and
    // `b.dependency("...")` use; it is local to this package.
    .dependencies = .{
        .json = .{
            .url = "https://pkg.k2-lang.org/json/2.3.1.tar.gz",
            // Content hash of the fetched archive. Fetch is rejected unless the
            // bytes hash to exactly this value — deps are content-addressed.
            .hash = "k2pkg-2x8f3a1c9d7e6b40a2c15f8e9013d4a7b6c2e1f085a3d9c7e",
        },
        // A path dependency for local development / monorepos.
        .geomath = .{
            .path = "../geomath",
        },
    },
};
```

### 7.2 Content-addressed, not name-resolved

k2 dependencies are **content-addressed**: a dependency is identified by the
cryptographic hash of its fetched contents, not by a mutable name-and-version
lookup against a central index at build time. The consequences:

- **Reproducibility.** A given `(url, hash)` pair always yields byte-identical
  source, or the build fails. There is no "the registry changed under me."
- **Integrity.** The hash is verified after fetch; a tampered or corrupted
  archive cannot be substituted for the expected one.
- **A flat, deduplicated store.** Fetched packages live in a global,
  content-addressed cache keyed by hash. Two projects depending on the same
  hash share one copy; two different versions coexist without conflict.

The version string in a manifest is human-facing metadata and the basis for the
resolver's *selection*; the **hash** is what actually pins the bytes. The package
registry (`pkg.k2-lang.org` in the examples) is a convenience for discovery and
hosting, not a trusted authority — the hash is the trust root.

### 7.3 The lockfile

Resolving the manifest produces a **lockfile** (`k2.lock`) recording, for the
whole transitive dependency graph, the exact `(name, url, hash)` triples chosen.
The lockfile is committed to version control and is what `b.dependency(...)`
reads at build time. Updating a dependency is an explicit command
(`k2 pkg update`) that rewrites the lock; an ordinary build never silently moves
a dependency.

### 7.4 Fetching is the only non-comptime part

Evaluating a `k2.pkg` is pure comptime and performs no I/O — it just produces the
`Package` value. *Fetching* the archives named by `url` is a separate,
explicit toolchain step (`k2 pkg fetch`, run automatically by `k2 build` when a
locked dependency is absent from the cache). This keeps the comptime sandbox
honest: comptime code never reaches the network, consistent with **No ambient
authority** applied to the build engine itself.

---

## 8. Cross-compilation: target triples

Cross-compilation is **first-class**, not an afterthought (charter backend
strategy). The target is a single build parameter — a **target triple** of the
form `arch-os-abi` — and selecting a non-host triple is the entire ceremony.

Because `build(b)` already threads `target` into every artifact via
`b.standardTarget()`, the build script needs no special cases for
cross-compilation. The user simply names a triple:

```sh
# Native build for the host.
k2 build

# Cross-compile the same project for 64-bit Linux (musl), aarch64 macOS, and Windows.
k2 build -Dtarget=x86_64-linux-musl
k2 build -Dtarget=aarch64-macos
k2 build -Dtarget=x86_64-windows-gnu -Doptimize=ReleaseFast
```

The triple has up to three fields:

| Field  | Examples                                   |
|--------|--------------------------------------------|
| `arch` | `x86_64`, `aarch64`, `riscv64`, `wasm32`   |
| `os`   | `linux`, `macos`, `windows`, `freebsd`, `wasi` |
| `abi`  | `gnu`, `musl`, `msvc`, `none` (freestanding) |

What makes it trivial in practice (per the charter):

- The toolchain **bundles libc headers and stubs** for common targets, so you do
  not assemble a cross-toolchain by hand.
- C interop travels through the same cross-aware driver: `extern` declarations
  and the integrated C-translation path are resolved against the *target's*
  system libraries, not the host's.
- The same typed, monomorphized MIR feeds both backends, so a cross-build differs
  from a native build only in the backend's target configuration and the linked
  system libraries — semantics are identical.

A `Target` value is also a comptime value inside `build(b)`, so a build script
can branch on it (e.g. add a platform-specific source file only on `windows`)
using ordinary `if`/`switch` over `target.os`.

---

## 9. The toolchain commands

All build, run, and test workflows go through three subcommands, each of which
evaluates `build.k2` and then executes the appropriate step graph.

### 9.1 `k2 build`

Compiles the project. With no step name it runs the default `install` step,
producing every artifact passed to `b.installArtifact(...)`. A named step runs
that step and its dependencies.

```sh
k2 build                                  # default install step (Debug)
k2 build -Doptimize=ReleaseFast           # optimized native build
k2 build -Dtarget=aarch64-linux-gnu       # cross-compile
k2 build -Dwith-tls=true                  # set a custom build option
```

### 9.2 `k2 run`

Builds the executable artifact and runs it, forwarding any arguments after `--`
to the program. It is shorthand for building and invoking the `run` step you
wired in §6; for single-executable projects the toolchain infers it.

```sh
k2 run                                     # build + run the default executable
k2 run -- --input data.json --verbose      # args after `--` go to the program
k2 run -Doptimize=ReleaseSafe -- --bench    # run an optimized-but-checked build
```

Recall that the executable's `main` has the fixed capability signature
`pub fn main(sys: *System) !void` — the program receives its sole authority,
`*System`, at startup, and nothing reaches the OS without going through it.

### 9.3 `k2 test`

Builds and runs the project's test steps. Each `test { ... }` block (and each
`test "name" { ... }` block) in the modules reachable from a test artifact's
`root_source` is compiled into a test runner and executed.

```sh
k2 test                                    # run all test steps
k2 test -Dtarget=wasm32-wasi               # cross-compile tests and run under WASI
```

A `test` block is part of the language (it is a keyword, §00) and is compiled
**only** in test builds — in ordinary `k2 build`/`k2 run` it is skipped
entirely, so tests cost nothing in shipped binaries:

```k2
const std = @import("std");
const math = @import("math.k2");

test "square doubles exponent" {
    try std.testing.expectEqual(@as(f64, 9.0), math.square(3.0));
}

test "Vec2 length squared" {
    const v = math.Vec2{ .x = 3.0, .y = 4.0 };
    try std.testing.expectEqual(@as(f64, 25.0), v.lengthSquared());
}
```

Because tests are ordinary k2 functions, they receive capabilities exactly like
any other code: the test runner hands each test the capabilities it needs (a
testing allocator that detects leaks, a fake clock, a seeded RNG), making tests
**deterministic and sandboxed** by construction — the testing payoff of **No
ambient authority**.

### 9.4 `k2 pkg`

Manages dependencies declared in `k2.pkg`:

```sh
k2 pkg fetch        # download + verify any locked deps missing from the cache
k2 pkg update       # re-resolve manifests, rewrite k2.lock to newest allowed
k2 pkg hash <url>   # fetch an archive and print its content hash (for manifests)
```

`k2 build`/`k2 run`/`k2 test` invoke `fetch` implicitly when a locked dependency
is absent, but never `update` — moving a dependency is always an explicit act.

---

## 10. A complete minimal project layout

Putting the pieces together, a small library-plus-CLI project looks like this:

```
geocli/
├── build.k2            # the build graph (§6)
├── k2.pkg              # the manifest: name, version, deps (§7)
├── k2.lock             # resolved, content-addressed dependency pins (§7.3)
├── src/
│   ├── root.k2         # library root module — public API (pub re-exports)
│   ├── main.k2         # executable root module — `pub fn main(sys: *System)`
│   ├── shapes.k2       # internal module, reached via @import("shapes.k2")
│   └── vec.k2          # internal module
└── tests/
    └── integration.k2  # integration-test root, imports the "geo" module
```

The corresponding `src/main.k2`:

```k2
const std = @import("std");
const geo = @import("geo");       // the library, wired in by build.k2
const json = @import("json");     // the dependency, wired in by build.k2

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    const c = geo.Circle{ .radius = 2.0 };
    try out.print("area = {d}\n", .{c.area()});

    // Read args through the env capability — no ambient access.
    const args = try sys.env.args(sys.heap);
    defer sys.heap.free(args);
    try out.print("argc = {d}\n", .{args.len});

    _ = json;   // (used elsewhere in a real program)
}
```

Every import name here (`std`, `geo`, `json`) is established by the build script,
every effect (`sys.io`, `sys.heap`, `sys.env`) is a capability threaded from
`*System`, and every byte of the build description is k2 you can read. That is
the chapter's promise: organization, distribution, and the build itself are all
the same small language, with nothing happening behind your back.

---

### See also

- **§01 — Lexical structure:** the `.k2` extension, `@import` and `@`-builtins as
  tokens, doc comments, and the `build.k2` distinctive feature.
- **§02 — Types:** files as implicit structs; `struct`/`enum`/`union` and the
  `opaque` type used at hard module boundaries and for C interop.
- **§04 — Functions:** `pub` on functions and struct methods; the fixed
  `pub fn main(sys: *System) !void` entry-point signature.
- **§05 — Memory and allocators:** the `Allocator` capability that artifacts and
  tests receive, and the testing allocator that detects leaks.
- **§09 — comptime and reflection:** the evaluation model that runs `build.k2`
  and `k2.pkg`, and the `@typeInfo`/`@Type` machinery behind module reflection.
