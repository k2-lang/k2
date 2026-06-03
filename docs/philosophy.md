# The Philosophy of k2

> **Total control over the machine, with zero waste.**

This document is the definitive statement of why k2 exists, what it believes,
and what it refuses to do. Everything else in the language — every keyword,
every builtin, every standard-library signature — is downstream of the ideas
written here. If a future feature contradicts this document, the feature is
wrong.

---

## 1. The name: Kardashev Type II

The Kardashev scale measures a civilization by the energy it can harness. A
**Type II** civilization captures the *total* energy output of its star — not a
fraction, not what happens to leak out, but every joule the star produces,
directed deliberately.

`k2` is a promise framed in those terms, applied to the relationship between a
program and the machine it runs on:

- **Complete.** All of the machine's power is available to you. Nothing is held
  back behind a runtime, a garbage collector, or a privileged standard library.
- **Efficient.** Nothing is wasted. An abstraction that costs more than the code
  a careful programmer would write by hand is a bug, not a convenience.
- **Deliberate.** Nothing happens behind your back. Every joule the CPU spends
  is spent because something visible in your source told it to.

The lowercase, two-character name is itself part of the message: a **small
surface area over immense underlying power**. You should be able to hold the
entire language in your head, and from that small vocabulary direct the full
output of the machine.

This is not a metaphor about ambition. It is a concrete engineering constraint.
Every section below ties back to a single mandate:

> **The speed mandate:** a k2 program must be able to reach the performance of
> equivalent hand-written, runtime-free native code — and the language must
> never silently put anything between you and that goal.

---

## 2. The shape of the language, in one breath

Before the principles, here is the texture, so the code below reads as ordinary:

```k2
const std = @import("std");

/// Program entry point. `sys` is the root capability handle —
/// the sole source of authority in a k2 program.
pub fn main(sys: *System) !void {
    // No ambient stdout: we take it from the io capability.
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

`const`/`var` bindings, `fn` for functions, postfix type modifiers (`?T`, `!T`,
`*T`, `[]T`, `[N]T`), `@`-sigil builtins for compile-time work, and a single
explicit entry point that receives all authority as a parameter. That is the
whole frame. Now the reasons.

---

## 3. The philosophy pillars

k2 has seven load-bearing beliefs. Each is stated, motivated, and then shown in
code. Each ends with how it serves the speed mandate, because none of them are
aesthetic preferences — they are all in service of *total, predictable control*.

### Pillar 1 — No hidden control flow

**Reading k2 code, you can see exactly what runs.**

There are no exceptions that unwind through frames you didn't write. There are
no destructors firing as scopes close. There is no operator overloading that
secretly dispatches into user code, and no implicit coercion that calls a
conversion function you never named. A function call looks like a function call.
*Everything that is not a function call is plain data movement.*

The complete set of non-linear control flow is closed and small:

> `if` / `else`, `while`, `for`, `switch`, `try`, `catch`, `defer`,
> `errdefer`, `orelse`, `return`, `break`, `continue`, `unreachable`.

If you do not see one of those keywords (or a function call), control is falling
straight through, top to bottom. That is the entire model.

Consider cleanup. In a language with destructors, the code that runs when a
scope exits is *invisible* — it lives in a type definition somewhere else. In
k2, cleanup is a statement you can point at:

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    const buf: []u32 = try alloc.alloc(u32, 16);
    // The release is written, in this scope, where you can see it.
    defer alloc.free(buf);

    for (buf, 0..) |*slot, i| {
        slot.* = @intCast(i * i);
    }

    const out = sys.io.stdout();
    try out.print("buf[15] = {d}\n", .{buf[15]});
}
```

Even error propagation, the one piece of sugar k2 allows, is explicitly
spelled. `try expr` is the *only* shorthand in the language, and it expands to a
visible early `return`:

```k2
// `try out.print(...)` means exactly:
//   const r = out.print(...) catch |e| return e;
// One token, but no hidden frame walking — just an ordinary return.
```

**Why this serves speed.** Hidden control flow has a cost model you cannot see,
and therefore cannot budget for. Exceptions impose unwind tables and inhibit
optimization across call sites that *might* throw. Implicit destructors and
overloaded operators turn a single line into an unknown amount of work. When the
only control flow is the explicit set above, the compiler — and you — know the
exact cost of every line. Error handling in k2 has *the same cost model as any
other branch*, because that is literally what it compiles to.

### Pillar 2 — No hidden allocation

**The language and standard library never allocate heap memory on your behalf.**

Any function that may touch the heap takes an explicit `Allocator` capability as
a parameter. There is no global allocator, no implicit growable-string magic, no
autoboxing of values onto the heap. If a value lives on the heap, the code that
put it there is visible *at the call site*.

```k2
const std = @import("std");

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

        pub fn deinit(self: *Self) void {
            self.alloc.free(self.items);
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

Notice that `List` cannot grow without an `Allocator`. It does not reach for a
default — there isn't one. The owner of the list decided where its memory comes
from, and that decision is right there in `init`'s signature.

Because `Allocator` is an *interface* — a struct of function pointers plus a
context pointer — the call site also chooses the *strategy*: an arena that frees
everything at once, a fixed buffer with no syscalls at all, a page allocator, or
the general-purpose allocator that detects leaks in safe builds. The library
code above is identical for all of them.

```k2
pub fn main(sys: *System) !void {
    // The heap capability picks the strategy; List never assumes one.
    var nums = List(u32).init(sys.heap);
    defer nums.deinit();
    try nums.push(40);
    try nums.push(2);
    const out = sys.io.stdout();
    try out.print("sum = {d}\n", .{nums.items[0] + nums.items[1]});
}
```

**Why this serves speed.** Hidden allocation is the most common reason
high-level code is slow: a string concatenation that quietly mallocs, a boxed
closure, an autogrown collection inside a hot loop. When every allocation is
visible and parameterized, you can *eliminate* it — swap in an arena, reuse a
buffer, move it out of the loop — without fighting the language. The fastest
allocation is the one that never happens, and k2 makes every one of them appear
in your source where you can decide to remove it.

### Pillar 3 — No ambient authority

**k2 extends "no hidden allocation" to *all* power over the outside world.**

This is k2's signature idea. Allocators are not special; they are just the first
member of a larger family. I/O, the clock, randomness, the environment, the
filesystem, and the network are *capability values* that must be passed
explicitly, threaded down from the program root.

There is no `std.print` that secretly grabs stdout. There is no `time.now()`.
There is no `os.getenv()` that works without being handed the environment. The
single source of all authority is the `*System` handed to `main`:

```k2
pub fn main(sys: *System) !void { ... }
```

From `sys` you obtain narrower capabilities, and each is a distinct value:

| Capability    | Grants                                   |
| ------------- | ---------------------------------------- |
| `sys.heap`    | an `Allocator`                           |
| `sys.io`      | stdin / stdout / stderr and file opening |
| `sys.clock`   | monotonic and wall-clock time            |
| `sys.random`  | a CSPRNG seed / handle                   |
| `sys.env`     | environment variables and process args   |
| `sys.net`     | network access                           |

The consequence is profound: **a function's signature is an honest, complete
account of what it can do.**

```k2
// This function can compute. It cannot read the clock, open a socket,
// allocate, or print. The signature proves it — there is no global it
// could reach for. Pure by construction.
fn checksum(data: []const u8) u32 {
    var acc: u32 = 2166136261;
    for (data) |byte| {
        acc = (acc ^ byte) *% 16777619;
    }
    return acc;
}

// This one can allocate, and *only* allocate. It cannot touch I/O or
// the network, because it was never handed those capabilities.
fn dup(alloc: Allocator, data: []const u8) ![]u8 {
    const copy = try alloc.alloc(u8, data.len);
    for (data, 0..) |byte, i| copy[i] = byte;
    return copy;
}
```

This makes k2 code:

- **Sandboxable.** Hand a function a mock `System` and it can do nothing you did
  not permit — there is no back channel to the OS.
- **Deterministically testable.** Hand it a fake clock and a seeded RNG and the
  same code becomes reproducible:

  ```k2
  const std = @import("std");

  // The same function under test, given a faked System, is deterministic:
  // no wall-clock dependence, no global RNG, no ambient I/O.
  test "report is reproducible under a fake System" {
      var fake = std.testing.fakeSystem(.{
          .clock = .{ .fixed_nanos = 1_700_000_000 },
          .random = .{ .seed = 42 },
      });
      const line = try renderReport(&fake.system);
      try std.testing.expectEqualStrings("t=1700000000 r=...\n", line);
  }
  ```

- **Auditable.** To know whether a subsystem can reach the network, you read its
  parameter types. You do not have to grep the whole transitive call tree for a
  rogue global.

**Why this serves speed.** Capabilities are plain structs of function pointers
plus a context pointer, so a capability call costs exactly one indirect call —
and when the type is monomorphic, the optimizer *devirtualizes and inlines it
away to nothing*. You pay zero for the discipline in the common case. Just as
importantly, because effects are explicit, the compiler knows which functions
are pure and can hoist, reorder, and cache their results aggressively. Ambient
authority would force the optimizer to assume any call might touch the world;
explicit capabilities free it to assume the opposite.

### Pillar 4 — comptime is the only metaprogramming

**Compile-time evaluation of ordinary k2 code is the single mechanism for
metaprogramming.**

There is no macro language. There is no template grammar. There is no separate
syntax for generics. Instead:

- Any expression can be forced to compile time with `comptime`.
- A parameter marked `comptime` must be supplied a compile-time-known value.
- `type` is itself a first-class comptime value, so functions take and return
  types.

Generics are therefore *just functions*. `List(comptime T: type) type` from
Pillar 2 is a function that runs at compile time and returns a struct type. The
same mechanism powers reflection, via the `@typeInfo` / `@Type` round-trip:

```k2
const std = @import("std");

/// Generate a field-by-field printer for ANY struct type at comptime.
/// No macros: this is ordinary k2 run by the compiler.
fn printFields(comptime T: type, out: anytype, value: T) !void {
    const info = @typeInfo(T);
    // Reject non-structs at compile time with a precise diagnostic.
    if (info != .Struct) {
        @compileError("printFields requires a struct, got " ++ @typeName(T));
    }
    // `inline for` unrolls over the comptime-known field list.
    inline for (info.Struct.fields) |field| {
        try out.print("{s} = {any}\n", .{ field.name, @field(value, field.name) });
    }
}

const Point = struct { x: i32, y: i32 };

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p = Point{ .x = 3, .y = 4 };
    try printFields(Point, out, p);
}
```

That serializer-style printer is a plain function. It uses the same `if`, the
same `for`, the same `@field` you would use in runtime code — it merely runs
during compilation. Learn the language once; you have learned its metaprogram.

**Why this serves speed.** Generics are monomorphized: each distinct `T`
produces fully concrete, specialized code with no runtime type dispatch and no
boxing. Reflection happens entirely at compile time, so `printFields` above
compiles down to a fixed sequence of `print` calls with *zero* runtime
introspection. Abstractions written this way collapse to exactly the code you'd
write by hand — which is the speed mandate restated. And because metaprogramming
is sandboxed comptime code that cannot perform I/O or runtime allocation, it
adds nothing to your binary except the values and types it produced.

### Pillar 5 — Errors are values

**Failures are ordinary values carried in error unions, not a separate
control-flow channel.**

An error set is a type: `error{ OutOfMemory, NotFound }`. A function that can
fail returns an error union — `E!T` with an explicit set, or `!T` with the set
inferred from the body. Error sets are comptime-mergeable and subset-comparable,
so libraries can reason about exactly which errors a callee can produce.

The operators are the explicit set from Pillar 1: `try` propagates, `catch`
handles, `errdefer` cleans up *only on the error path*.

```k2
const std = @import("std");

/// An explicit error set: error sets are types and are introspectable.
const ParseError = error{ Empty, NotANumber };

/// Parse `text` into a heap-owned, doubled copy of its u32 value.
/// Returns an error union combining allocation and parse errors.
fn parseDoubled(alloc: Allocator, text: []const u8) (ParseError || error{OutOfMemory})!*u32 {
    if (text.len == 0) return ParseError.Empty;

    const cell: *u32 = try alloc.create(u32);
    // Runs ONLY if a later step in this scope returns an error.
    errdefer alloc.destroy(cell);

    var value: u32 = 0;
    for (text) |ch| {
        if (ch < '0' or ch > '9') return ParseError.NotANumber;
        value = value * 10 + @as(u32, ch - '0');
    }
    cell.* = value * 2;
    return cell;
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // `catch` supplies a fallback by handling the error value.
    const ptr = parseDoubled(sys.heap, "21") catch |err| {
        try out.print("parse failed: {s}\n", .{@errorName(err)});
        return;
    };
    defer sys.heap.destroy(ptr);

    try out.print("doubled = {d}\n", .{ptr.*});
}
```

Note `errdefer alloc.destroy(cell)`: it releases the half-built resource if and
only if a *later* step fails. This is the standard k2 pattern for cleanly
unwinding a partially-constructed value without a destructor and without
exceptions.

One critical distinction: **programmer mistakes are not errors.** Out-of-bounds
access, integer overflow, and reaching `unreachable` are *bugs*, not recoverable
conditions. In safe builds they trigger an explicit `@panic`; in `ReleaseFast`
they are undefined behavior. They are never silently converted into an error
value you might accidentally ignore.

**Why this serves speed.** Because an error is just a small integer tag in a
union — no allocation, no context object, no stack trace captured by default —
error handling costs a branch and a move. There are no unwind tables, no
landing pads, no inhibited inlining around fallible calls. `try` is a plain
conditional return. The "happy path" and the "error path" are the same kind of
code with the same cost model, so making your code robust does not make it slow.

### Pillar 6 — One obvious way, small surface

**k2 favors a small keyword set and a single idiomatic spelling for each
concept.**

Where two features would overlap, k2 ships one. The boolean operators are words
— `and`, `or`, `not` — leaving the symbolic `!` to mean exactly one thing:
error unions. There is one way to declare an immutable binding (`const`), one
way to declare a mutable one (`var`), one growable-list idiom, one allocation
pattern (`alloc` paired with `defer free`). Readability and precise
communication of intent outrank cleverness and terseness.

```k2
// Word-based booleans; `!` is reserved strictly for error unions.
fn inRange(x: i32, lo: i32, hi: i32) bool {
    return x >= lo and x <= hi and not (x == lo and x == hi);
}
```

The payoff is the lowercase-name promise made good: a reader can hold the whole
language in their head. There is no dialect to learn, no "which of the five
string types do I use here," no surprising interaction between two features that
solve the same problem.

**Why this serves speed.** A small, orthogonal language is a small, predictable
compiler. Fewer overlapping features means fewer special cases in the optimizer
and fewer places where an abstraction's cost is unclear. When there is one
obvious way to express something, that way can be the one the compiler is taught
to make fast — and you never accidentally pick the slow spelling because there
isn't one. Predictability *is* performance: code whose cost you can see is code
whose cost you can minimize.

### Pillar 7 — Native speed, no runtime

**k2 compiles to native machine code with no garbage collector, no green-thread
scheduler, and no language runtime linked into your binary.**

This is the pillar the other six exist to protect. Zero-cost abstractions are a
**hard requirement**, not an aspiration: a k2 abstraction must compile to the
same code a careful programmer would write by hand. If it does not, the
abstraction is broken.

Concretely, that means:

- No GC pauses, because there is no GC. Memory is managed manually through the
  `Allocator` capability, with safe builds catching leaks and use-after-free.
- No scheduler. Concurrency is library-provided over OS threads, with an
  `Executor`/`ThreadPool` capability passed explicitly — never globally
  available.
- No hidden reference counting and no implicit destructors. Cleanup is always
  the visible `defer`/`errdefer` from Pillar 1.
- C interop and cross-compilation are first-class: `extern` for C symbols, an
  integrated C-translation path, and a target triple that is a build parameter,
  with bundled libc stubs for common targets.

The dual backend makes this practical at both ends of the workflow. Debug builds
use **Cranelift** for near-instant compiles and a tight feedback loop, paired
with the full safety toolkit. Release builds lower the same monomorphized MIR to
**LLVM**, where every capability indirection and generic instantiation collapses
to hand-written-equivalent machine code. The build-mode ladder lets you choose:

| Mode          | Backend   | Safety checks            |
| ------------- | --------- | ------------------------ |
| `Debug`       | Cranelift | full (bounds, overflow, leak/UAF) |
| `ReleaseSafe` | LLVM      | full, but optimized      |
| `ReleaseFast` | LLVM      | stripped (UB on violation) |

**Why this serves speed.** This pillar *is* the speed mandate. The other six
pillars are the disciplines that make a runtime unnecessary: with no hidden
control flow, no hidden allocation, no ambient authority, and errors-as-values,
there is nothing left for a runtime to *do*. What remains is your code, compiled
straight to the machine — every joule of the star, yours to direct.

---

## 4. How k2 differs from C, C++, Rust, and Go

k2 is Zig-inspired in spirit, but its sharpest contrasts are with the four
languages most programmers reach for when they want native speed. Each
comparison is framed against the pillars above.

### Versus C

C shares k2's "no runtime, no hidden allocation" instinct, and that kinship is
real. The differences are about *safety without cost* and *honesty of
signatures*:

- **Ambient authority.** In C, any function can `malloc`, `printf`, `getenv`, or
  `open` — the entire OS is a global namespace. A C function's signature tells
  you almost nothing about what it can do. In k2, effects are capabilities, so a
  signature is a complete account of a function's powers.
- **Errors.** C signals failure through sentinel return values, `errno`, and
  convention. k2 has typed, introspectable error unions with `try`/`catch`,
  carrying the same near-zero cost as a C sentinel check but with exhaustiveness
  and propagation built in.
- **Metaprogramming.** C's only metaprogram is the text-substituting
  preprocessor. k2 replaces it with `comptime`: real code, type-checked, with
  reflection — no `#define` token soup.
- **Safety.** C offers no bounds or overflow checks. k2's safe builds insert
  them and ship a leak/use-after-free-detecting allocator, then let you strip it
  all in `ReleaseFast` for identical-to-C performance.

### Versus C++

C++ chases zero-cost abstractions too, but reaches them through a far larger and
more *hidden* machine — exactly the things Pillars 1 through 6 forbid:

- **Hidden control flow.** Constructors, destructors, exceptions, RAII, and
  operator overloading mean a single line of C++ can run an unbounded amount of
  code you cannot see. k2 forbids all of it: cleanup is an explicit `defer`, and
  the only non-linear control flow is the closed keyword set.
- **Exceptions vs. values.** C++ exceptions impose unwind tables and a cost
  model that differs from ordinary branches. k2's errors are values with one
  uniform cost model.
- **Templates vs. comptime.** C++ templates are a second, Turing-complete
  language with its own arcane rules and famously inscrutable diagnostics. k2's
  `comptime` is *the same language*, with `@compileError` producing precise,
  source-located messages.
- **Surface area.** C++ is enormous and still growing; no one holds it all in
  their head. k2's Pillar 6 is a direct rejection of that trajectory.

### Versus Rust

Rust is k2's closest peer on safety and native speed, and the comparison is the
most nuanced. Both reject GC; both demand zero-cost abstractions. The
differences are philosophical:

- **Safety model.** Rust enforces memory and data-race safety statically with a
  borrow checker — a powerful but heavyweight system you must satisfy for *all*
  code. k2 chooses a lighter discipline: `*T` (exclusive) versus `*const T`
  (shared) by convention, runtime safety checks in Debug/ReleaseSafe, and the
  leak/UAF-detecting allocator — *not* a borrow checker. k2 trades some
  compile-time guarantees for a smaller language and a gentler model.
- **Ambient authority.** This is the decisive difference. Rust still exposes
  ambient OS access: `println!` reaches stdout, `std::time` reads the clock,
  `std::env` reads the environment — all without being passed in. k2 has **no**
  such globals; every effect is a capability threaded from `*System`. k2's code
  is sandboxable and deterministically testable *by construction*, which Rust's
  is not.
- **Macros vs. comptime.** Rust has `macro_rules!` *and* procedural macros — a
  separate metaprogramming language (often two). k2 has one mechanism,
  `comptime`, which is just k2.
- **Errors.** Both treat errors as values, which is shared common ground. k2's
  error sets are mergeable, subset-comparable types with `try`/`catch`/
  `errdefer`, closely related in spirit to Rust's `Result` and `?`, but with no
  trait machinery behind them.

### Versus Go

Go optimizes for simplicity and fast builds, goals k2 shares — but Go buys them
with a runtime, which Pillar 7 categorically rejects:

- **Runtime and GC.** Go ships a garbage collector and a goroutine scheduler
  linked into every binary, with the pauses and unpredictability that implies.
  k2 has no GC and no scheduler; concurrency is explicit OS threads behind a
  capability.
- **Hidden allocation.** In Go, `append`, string concatenation, interface
  conversions, and closures allocate implicitly and silently. k2's Pillar 2
  forbids this outright — every allocation takes an explicit `Allocator`.
- **Ambient authority.** Go's `fmt.Println`, `time.Now`, `os.Getenv`, and `net`
  are global. k2 threads all of them as capabilities.
- **Error ergonomics.** Go's `if err != nil` is values-as-errors, like k2, but
  without typed error sets, `try` propagation, or `errdefer` cleanup.
- **Metaprogramming.** Go deliberately had almost none for years and now has
  constrained generics. k2's `comptime` is fully general type-level computation
  while remaining one mechanism.

A one-line summary:

> **C** gives you control without safety. **C++** gives you power with hidden
> machinery. **Rust** gives you safety through a heavyweight checker and a
> larger language. **Go** gives you simplicity at the price of a runtime. **k2**
> gives you total, visible control — safety you can strip, authority you must
> pass, and a runtime that does not exist.

---

## 5. What we deliberately leave out, and why

A language is defined as much by its omissions as its features. Each of the
following was considered and rejected. None is an oversight; each would violate a
pillar and, through it, the speed mandate.

### Exceptions and stack unwinding

*Violates Pillar 1 (no hidden control flow) and Pillar 5 (errors are values).*
Exceptions make any call a potential non-local jump, impose unwind tables, and
give error handling a different cost model than ordinary branches. k2 uses error
unions, which propagate via the explicit, visible `try`.

### A garbage collector

*Violates Pillar 7 (no runtime).* A GC means pauses, a runtime in every binary,
and a cost model you do not control. k2 manages memory manually through the
`Allocator` capability, catching leaks and use-after-free in safe builds rather
than preventing them with a collector.

### Implicit destructors / RAII

*Violates Pillar 1.* A destructor is control flow you cannot see at the call
site, hidden in a type definition. k2 makes cleanup an explicit statement:
`defer` for the normal path, `errdefer` for the error path. Nothing runs on
scope exit that you did not write.

### Operator overloading

*Violates Pillar 1.* Overloaded operators let `a + b` dispatch into arbitrary
user code, turning innocent-looking arithmetic into hidden function calls with
hidden costs. In k2, operators mean exactly what they say, and a function call
is the only way to invoke user code.

### Implicit numeric and type coercions

*Violates Pillar 1.* Silent widening, narrowing, and conversion functions hide
both control flow and cost. k2 requires explicit `@as` for lossless coercions
and `@intCast` for checked narrowing, so every conversion is visible — and the
checked ones panic on loss in safe builds rather than corrupting data silently.

### A macro system and a separate generics grammar

*Violates Pillar 4 (comptime is the only metaprogramming) and Pillar 6 (small
surface).* A preprocessor or template language is a second language to learn,
with its own diagnostics and failure modes. k2 has exactly one metaprogramming
mechanism — `comptime` — which is ordinary k2.

### A built-in async runtime / green-thread scheduler

*Violates Pillar 3 (no ambient authority) and Pillar 7 (no runtime).* A baked-in
scheduler is both a runtime and an ambient effect. k2's async is colorless,
stackless, and lowered at compile time: an `async` call returns a `Frame` whose
storage the *caller* owns (no hidden allocation), and the event loop that drives
frames is an explicit capability obtained from `System`. The same function runs
blocking or evented depending on the capability it is handed.

### Global state and ambient OS access

*Violates Pillar 3.* No `std.print`, no `time.now()`, no `os.getenv()` that
works without a capability. Every effect flows from `*System`. This is the price
of sandboxability, determinism, and audit-by-signature — and k2 pays it
gladly.

### A borrow checker

*Violates Pillar 6 (small surface).* It is a powerful tool, but a heavy one that
shapes the entire language around itself. k2 chooses a lighter discipline —
`*T` versus `*const T`, runtime safety checks, and the leak/UAF-detecting
allocator — accepting fewer static guarantees in exchange for a language you can
hold in your head.

### Multiple inheritance, classes, and a type hierarchy

*Violates Pillar 6.* k2 has `struct`, `enum`, and tagged `union(enum)`, plus
comptime-computed types for polymorphism. There is no inheritance, no vtable
hierarchy, and no method resolution order to reason about. Composition and
explicit interfaces (structs of function pointers, like `Allocator`) cover the
ground with no hidden dispatch.

---

## 6. The through-line

Every choice in this document points the same direction:

1. **No hidden control flow** means you can see every instruction's cost.
2. **No hidden allocation** means you can eliminate every byte you didn't ask
   for.
3. **No ambient authority** means the compiler knows what's pure and you know
   what's safe.
4. **comptime-only metaprogramming** means abstractions vanish at compile time.
5. **Errors as values** means robustness costs a branch, not a runtime.
6. **One obvious way** means a predictable language and a predictable compiler.
7. **No runtime** means nothing stands between your code and the machine.

Together they make the Kardashev Type II promise concrete: **complete,
efficient, deliberate control over every joule the CPU spends.** Total control
over the machine, with zero waste.
