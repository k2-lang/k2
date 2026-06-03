# k2 Examples

> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

A guided tour of k2 through six small, complete programs. Each file is real,
idiomatic k2 you can read top to bottom; together they cover the language's
defining ideas: **no ambient authority**, **no hidden allocation**, **no hidden
control flow**, **errors as values**, and **comptime as the only
metaprogramming**.

Every example shares the same vocabulary. There is exactly one way to print
(through the `io` capability), one way to allocate (through an `Allocator`
capability), one error mechanism (`error` sets with `try`/`catch`/`errdefer`),
and one metaprogramming mechanism (`comptime`). Once you have read these six
files, you have seen most of the language.

---

## Running the examples

The examples are built by [`build.k2`](build.k2), which is itself an ordinary k2
program executed by the compiler's comptime engine — there is no second build
language.

```sh
# Build everything (Debug: Cranelift + full safety toolkit).
k2 build

# Build and run one example.
k2 build run -Dexample=hello
k2 build run -Dexample=allocators

# Compile and run every `test { ... }` block across the examples.
k2 build test

# Optimized native build, or a cross-compile.
k2 build -Doptimize=ReleaseFast
k2 build -Dtarget=aarch64-linux-gnu
```

For a single file you can also skip the build graph entirely:

```sh
k2 run examples/hello.k2
k2 test examples/errors.k2
```

Every executable shares the fixed entry-point signature
`pub fn main(sys: *System) !void`: the program receives its sole authority,
`*System`, at startup, and nothing reaches the operating system without going
through it.

---

## The examples

| File | Concepts | One-line summary |
| --- | --- | --- |
| [`hello.k2`](hello.k2) | capabilities, `*System`, `sys.io` | Hello world — there is no ambient stdout. |
| [`allocators.k2`](allocators.k2) | `Allocator`, `defer`/`errdefer`, GPA, `ArrayList`, arena | Explicit allocation with leak detection and bulk cleanup. |
| [`generic_list.k2`](generic_list.k2) | `comptime` generics, `@This()`, `realloc` | A generic container as a function from a type to a type. |
| [`errors.k2`](errors.k2) | `error` sets, `try`, `catch`, `errdefer`, `\|\|` | Failures are values; cleanup and propagation are visible. |
| [`comptime_reflection.k2`](comptime_reflection.k2) | `@typeInfo`, `inline for`, `@field`, `@compileError` | Generate a struct printer and a serializer at compile time. |
| [`build.k2`](build.k2) | `*Build`, artifacts, options, steps | A realistic build description, written in k2 itself. |

---

### `hello.k2` — capability-based stdout

The smallest complete program, and the first place k2 differs from most
languages. There is **no `std.print` that secretly grabs stdout**. The writer is
taken from the `io` capability, which is reached only through the `*System`
handed to `main`:

```k2
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

What to notice:

- **`sys` is the sole authority.** A function that was never handed `sys.io`
  literally cannot print. Effects are visible in signatures.
- **`!void`** is an error union: `main` may fail, and an unhandled error becomes
  a nonzero process exit with the error name printed.
- **`print` takes a format string and an argument tuple.** `{s}` formats a
  string, `{d}` a decimal integer; an empty argument list is `.{}`.
- **stderr is a separate, equally-explicit writer** (`sys.io.stderr()`) — never
  ambient.

---

### `allocators.k2` — explicit allocation, cleanup, and leak detection

k2 **never allocates heap memory on your behalf.** Any code that touches the
heap takes an `Allocator` capability as a parameter, so a signature is an honest
account of whether a function can allocate. This example walks four patterns:

1. **A leak-checking root allocator.** A `GeneralPurposeAllocator` is the
   program-wide allocator; in safe builds it detects leaks, double-frees, and
   use-after-free. Its `deinit` turns a forgotten `free` into a deterministic
   failure:

   ```k2
   var gpa = std.heap.GeneralPurposeAllocator.init(sys);
   defer {
       const leaked = gpa.deinit();
       if (leaked) @panic("memory leak detected at shutdown");
   }
   const alloc = gpa.allocator();
   ```

2. **A raw slice paired with its release.** The idiom is to write the `defer` on
   the line immediately after the allocation, so the lifetime is local and
   obvious: `const buf = try alloc.alloc(u32, 16); defer alloc.free(buf);`.

3. **`std.ArrayList`**, the canonical growable container. It stores the allocator
   it was handed and frees with that same allocator on `deinit`; `append` may
   fail with `OutOfMemory`, propagated by `try`.

4. **An arena** for bulk, single-shot cleanup: many allocations, one `deinit`
   that frees the whole region at once — no per-object frees.

The file also shows **ownership transfer**: `squares(alloc, n)` allocates and
hands ownership back, and the caller honors the contract with its own `defer`.
The `test` block uses `std.testing.allocator`, which fails the test if any
allocation leaks.

---

### `generic_list.k2` — a generic container via comptime

k2 has **no separate generics grammar, no templates, and no macros.** Generics
are ordinary functions that take `type` values at compile time and return a
`type`:

```k2
pub fn List(comptime T: type) type {
    return struct {
        const Self = @This();
        items: []T,
        len: usize,
        alloc: Allocator,
        // init / deinit / push / get ...
    };
}
```

What to notice:

- **`List(u32)` is an ordinary call evaluated by the compiler.** It returns a
  concrete struct type used exactly like a hand-written one, and it is cached:
  two `List(u32)` values are the same type and interoperate.
- **`@This()`** names the struct under construction, so methods can refer to
  their own type before it has a name.
- **The same definition, two element types.** `main` instantiates both
  `List(u32)` and `List([]const u8)`; they are distinct, separately-monomorphized
  types with zero runtime cost.
- **No hidden allocation.** `push` grows via `realloc` and `try`-propagates
  `OutOfMemory`; the container stores its allocator and frees with it in
  `deinit`. Absence (`get` past the end) is an optional `?T`, unwrapped with
  `orelse` — never an accidental null.

---

### `errors.k2` — error sets, `try`, `catch`, `errdefer`

In k2 **failures are values**, carried in error unions, not a stack-unwinding
exception channel. An error value is a small integer tag: no allocation, no
payload object. The only sugar is `try`, which expands to a visible early
`return`.

```k2
const ParseError = error{ Empty, NotANumber, Overflow };

fn parseDoubled(alloc: Allocator, text: []const u8) (ParseError || error{OutOfMemory})!*u32 {
    if (text.len == 0) return ParseError.Empty;
    const cell: *u32 = try alloc.create(u32);
    errdefer alloc.destroy(cell);   // runs ONLY on the error path
    // ... parse ...
    return cell;                    // success: errdefer is skipped, ownership transfers
}
```

What to notice:

- **Error sets are types**, written `error{ ... }`, and merge with `||`. The
  return type `(ParseError || error{OutOfMemory})!*u32` says exactly which
  failures are possible.
- **`try` propagates, `catch` handles.** The file shows both `catch` forms — a
  default value and a capture block that makes a decision and stops.
- **`errdefer` unwinds a half-built resource.** If a later step fails after
  `create`, the cell is freed; on success it survives and the caller takes
  ownership (and pairs it with `defer`).
- **Exhaustive `switch` over a captured error.** Because the capture has the
  operand's exact set, the `switch` must cover every member — add a member and
  the code stops compiling until you handle it.
- **`!` is reserved strictly for error unions; `not` is the boolean negation
  keyword.** They never overlap.
- **Mistakes are not errors.** The parser guards against integer overflow with
  an explicit `ParseError.Overflow` *value* rather than letting a safe-build
  overflow check panic.

The `test` blocks assert on error values directly with
`std.testing.expectError` — no exception machinery — and the testing allocator
proves the `errdefer` path leaks nothing.

---

### `comptime_reflection.k2` — generating code with `@typeInfo`

`comptime` is k2's **single metaprogramming mechanism**: ordinary k2 run by the
compiler. `@typeInfo` reflects a type into structured, matchable data, and
`inline for` unrolls over a struct's comptime-known field list to generate
per-field code — with **no macros and no runtime reflection**.

```k2
fn printFields(comptime T: type, out: anytype, value: T) !void {
    const info = @typeInfo(T);
    if (info != .Struct) {
        @compileError("printFields requires a struct, got " ++ @typeName(T));
    }
    inline for (info.Struct.fields) |field| {
        try out.print("  {s} = {any}\n", .{ field.name, @field(value, field.name) });
    }
}
```

The file builds two generic tools from reflection:

- **A struct printer** that works over any struct, with no per-type code.
- **A compact little-endian serializer** for structs of unsigned-integer fields.
  `serializedSize(T)` is a comptime function used both as a stack-array length
  and as a runtime bound; `serialize` unrolls into straight-line byte stores.

What to notice:

- **`@compileError` rejects bad type arguments at the call site** — a non-struct,
  or a struct with a non-unsigned field — turning a class of misuse into a
  precise compile error instead of a runtime surprise.
- **`@field(value, field.name)`** reads a field by its reflected, comptime name.
- **Zero runtime cost.** After monomorphization, `serialize(Packet, ...)` is a
  flat sequence of byte stores — exactly what you would write by hand for that
  concrete struct.

Because invalid instantiations do not compile, the tests can only exercise the
well-formed ones — which is precisely the guarantee the model provides.

---

### `build.k2` — the build system is k2 itself

A project's build is described in a `build.k2` file written in ordinary k2 and
executed by the compiler's comptime engine. There is **no second configuration
language** — targets, dependencies, cross-compilation triples, and build modes
are all expressed in the same language as the program being built.

```k2
const build = @import("build");

pub fn build(b: *Build) void {
    const target = b.standardTarget();      // honors -Dtarget=..., defaults to host
    const optimize = b.standardOptimize();  // Debug | ReleaseSafe | ReleaseFast
    // ... describe libraries, executables, tests, run steps ...
}
```

What to notice:

- **`b: *Build` is the build capability** — the build-time analogue of
  `*System`. There is no ambient build state; everything flows through `b`.
- **`build(b)` only *describes* a graph** of `Step` values; it compiles nothing
  itself and (like all comptime) performs no I/O. The toolchain then walks the
  requested step with the real backends — Cranelift for Debug, LLVM for Release.
- **Build options are comptime values in your code.** `addOption(...)` surfaces
  a `-D` flag to the compiled program via `@import("build_options")`, where the
  dead branch is eliminated — a zero-cost compile-time switch.
- **`inline for` builds artifacts declaratively** over a comptime list of
  example names, and **`if (target.os == .windows)`** branches on the resolved
  target with ordinary control flow — no platform DSL.

---

## How the examples relate to the charter pillars

| Pillar | Where you see it |
| --- | --- |
| **No ambient authority** | Every effect comes from `*System` in `hello`/`allocators`; `*Build` in `build.k2`. |
| **No hidden allocation** | `Allocator` is an explicit parameter everywhere in `allocators` and `generic_list`. |
| **No hidden control flow** | `try`/`catch`/`errdefer`/`defer` in `errors` and `allocators`; no exceptions, no destructors. |
| **Errors are values** | `error` sets, `\|\|` merging, and exhaustive `switch` in `errors`. |
| **comptime is the only metaprogramming** | `List(T)` in `generic_list`; `@typeInfo` codegen in `comptime_reflection`; the whole of `build.k2`. |
| **One obvious way, small surface** | One print path, one allocate path, one error mechanism, one build language. |
| **Native speed, no runtime** | Generated serializer compiles to byte stores; capabilities devirtualize when monomorphic. |

---

## Where to go next

- **Language specification** — the normative chapters under `../docs/spec/`,
  especially §05 (memory and allocators), §06 (error handling), §07 (comptime),
  and §08 (modules and the build system).
- **Philosophy** — `../docs/philosophy.md` for the reasoning behind the pillars.

Read the spec for the rules; read these examples for the feel. Between them you
have the whole of k2: complete, efficient, deliberate control over every joule
the machine spends.
