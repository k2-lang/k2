# 07 — Compile-Time Execution (`comptime`)

> Part of the **k2** language specification.
> **k2** — *Kardashev Type II.* Total control over the machine, with zero waste.

This chapter specifies `comptime`, the heart of k2. Compile-time evaluation of
ordinary k2 code is the language's **single** metaprogramming mechanism: there
is no macro language, no template syntax, and no separate generics grammar.
Where another language reaches for a preprocessor, a template engine, a
reflection runtime, or a code generator, k2 reaches for one thing — running
normal k2 code inside the compiler.

This is the **comptime is the only metaprogramming** pillar made concrete:

> Compile-time evaluation of ordinary k2 code is the single mechanism for
> metaprogramming. Generics are functions that take `type` values at comptime
> and return types. Types are first-class comptime values. One mechanism,
> learned once, used everywhere.

This chapter defines what runs at compile time and what does not (the
**comptime/runtime boundary**), how the `comptime` keyword forces evaluation,
how `comptime` parameters drive generics, the reflection round-trip
(`@typeInfo` → inspect → `@Type`), how to generate types and functions, and how
`@compileError` / comptime asserts turn invalid programs into precise
diagnostics. It closes with two non-trivial worked examples: a comptime-generated
binary serializer and a generic open-addressing hash map.

This chapter assumes the type system of [Chapter 02](02-types.md), the function
and generics machinery of [Chapter 04](04-functions.md), and the error model of
[Chapter 06](06-error-handling.md).

---

## Table of contents

1. [What "comptime" means](#1-what-comptime-means)
2. [The comptime/runtime boundary](#2-the-comptimeruntime-boundary)
3. [`comptime` as an expression and a block](#3-comptime-as-an-expression-and-a-block)
4. [`comptime` parameters and generics](#4-comptime-parameters-and-generics)
5. [`type` is a first-class comptime value](#5-type-is-a-first-class-comptime-value)
6. [Comptime control flow: `inline for` and `inline while`](#6-comptime-control-flow-inline-for-and-inline-while)
7. [Reflection: `@typeInfo`, `@TypeOf`, `@hasField`, `@field`](#7-reflection-typeinfo-typeof-hasfield-field)
8. [Synthesizing types with `@Type`](#8-synthesizing-types-with-type)
9. [Comptime asserts and `@compileError`](#9-comptime-asserts-and-compileerror)
10. [Evaluation rules, sandboxing, and termination](#10-evaluation-rules-sandboxing-and-termination)
11. [Worked example: a comptime-generated serializer](#11-worked-example-a-comptime-generated-serializer)
12. [Worked example: a generic open-addressing hash map](#12-worked-example-a-generic-open-addressing-hash-map)
13. [Summary](#13-summary)

---

## 1. What "comptime" means

**comptime** is the evaluation of ordinary k2 code by the compiler, during
compilation, to produce values that are baked into the program. It is not a
second language, not a preprocessor pass over text, and not a template
expansion engine. It is the *same* k2 — the same `const`/`var`, the same
`if`/`while`/`for`/`switch`, the same functions, the same structs — running in
a sandboxed interpreter inside the compiler instead of on the CPU at runtime.

Three facts, taken together, are the whole idea:

1. **Some values are known at compile time.** A literal `42`, the type `i32`,
   the result of `@sizeOf(Point)`, the length of a fixed array — all are fixed
   before the program runs. k2 calls these **comptime-known** values.
2. **`type` is one of those values.** A type like `i32` or `Point` or
   `[]const u8` is itself a value whose type is `type` (see
   [§2.1 of Chapter 02](02-types.md#21-type) and §5 below). Because types are
   values, functions can take them, compute with them, and return them.
3. **Ordinary code can be forced to run at compile time.** The `comptime`
   keyword marks an expression, a block, or a parameter as "evaluate this now,
   in the compiler." Generics, reflection, and code generation are all just
   ordinary k2 evaluated under that mark.

There is no fourth mechanism. Macros, templates, generics grammars, reflection
runtimes, and build-time code generators all collapse into this one capability:
**run k2 at compile time.** This is the payoff of the *one obvious way, small
surface* pillar — you learn comptime once and you have learned all of k2's
metaprogramming.

A first taste — a factorial computed entirely by the compiler, stored as a
constant in the binary:

```k2
/// An ordinary recursive function. Nothing about it is "comptime-only."
fn factorial(n: u64) u64 {
    return if (n <= 1) 1 else n * factorial(n - 1);
}

// The `comptime` keyword forces evaluation now: `table_size` is a constant
// folded into the program. No multiplication runs at runtime.
const table_size = comptime factorial(5);   // == 120, computed by the compiler
```

`factorial` is not a special "comptime function." It is a normal function that
*can* be called at runtime too. The `comptime` keyword at the call site is what
moves this particular evaluation into the compiler. This is the defining
property of k2 metaprogramming: the metaprogram is written in the object
language, with the same tools, and is debuggable and readable the same way.

---

## 2. The comptime/runtime boundary

Every k2 program is partitioned, at every point, into computation the compiler
performs (**comptime**) and computation the CPU performs (**runtime**). The
boundary is precise, mechanical, and — crucially for the *no hidden control
flow* pillar — visible in the source.

### 2.1 What is comptime-known

A value is **comptime-known** when the compiler can determine it without running
the program. The comptime-known values are exactly:

- literals (`42`, `3.14`, `true`, `"k2"`, enum literals like `.green`);
- any `const` initialized from a comptime-known expression;
- every value of type `type`, `comptime_int`, or `comptime_float` (these types
  exist *only* at compile time — see [§1.4 of Chapter 02](02-types.md#14-comptime_int-and-comptime_float));
- the result of any `@`-builtin whose arguments are comptime-known
  (`@sizeOf(T)`, `@typeInfo(T)`, `@TypeOf(x)`, `@hasField(T, name)`, …);
- the result of calling any function with comptime-known arguments inside a
  `comptime` context;
- a value bound to a `comptime` parameter (§4).

Everything else — a value read from a slice at a runtime index, a byte from
`sys.io`, the current `sys.clock` reading, a sum of two `var`s — is
**runtime-known**, computed when the program runs.

### 2.2 The two directions across the boundary

| Direction | Allowed? | How |
| --------- | -------- | --- |
| comptime → runtime | Always | A comptime value is materialized as a constant in the binary (a literal, a baked-in array, a monomorphized type's layout). |
| runtime → comptime | Never | A runtime value cannot flow *back* into a comptime context; the compiler does not have it. Trying to use a runtime value where a comptime one is required is a compile error. |

Comptime values flow *down* into runtime freely: that is how a comptime-computed
table, type, or constant ends up in the program. The reverse is impossible by
construction — the compiler is finished by the time the program runs, so a
runtime value can never be observed at comptime.

```k2
fn describe(sys: *System, n: usize) !void {
    const out = sys.io.stdout();

    // `n` is runtime-known (a parameter). This is fine — runtime code.
    try out.print("n = {d}\n", .{n});

    // Compile error: `n` is runtime-known, but @sizeOf needs a comptime type,
    // and an array length must be comptime-known.
    // const buf: [n]u8 = undefined;   // ERROR: use of runtime value `n`
}
```

### 2.3 The boundary is in the type, not in a separate phase of the file

There is no "comptime section" of a k2 file and no "`#if` block." The same
declaration, function, or expression can be comptime in one use and runtime in
another. `factorial` from §1 ran at comptime when called under `comptime`, and
would run at runtime when called normally. A function parameter is runtime
unless marked `comptime`; an array length is *always* comptime because the type
system requires it. The boundary is a property of how each value is *used*, and
the compiler reports a precise diagnostic the moment a use demands a
comptime-known value that is not available.

This is the *no hidden control flow* pillar applied to evaluation order: you can
always tell, by reading, which work the machine does at runtime and which work
the compiler already did. Nothing migrates across the boundary behind your back.

---

## 3. `comptime` as an expression and a block

The `comptime` keyword forces evaluation into the compiler. It has three uses:
on an **expression**, on a **block**, and on a **parameter** (§4). The first two
are covered here.

### 3.1 `comptime` on an expression

`comptime expr` requires `expr` to be evaluable at compile time and yields its
comptime-known result. If any part of `expr` depends on a runtime value, it is a
compile error.

```k2
// Forced comptime: the multiplication and the call run in the compiler.
const kib = comptime 1 << 10;            // 1024, a comptime_int
const fact5 = comptime factorial(5);     // 120, computed during compilation

// In a type position, comptime-ness is implied; the array length must be known.
const Lookup = [comptime factorial(4)]u8;   // [24]u8
```

In many positions `comptime` is **implied** and need not be written: array
lengths, the value supplied to a `comptime` parameter, and any operand of an
`@`-builtin that requires a comptime value are evaluated at comptime
automatically. You write the keyword explicitly when you want to *force* a
particular runtime-capable expression to fold, or to make the intent loud.

### 3.2 `comptime` on a block

`comptime { ... }` evaluates an entire block at compile time. Inside it, every
statement runs in the compiler; `var`s declared in the block are comptime
variables (mutable *during* compilation), and the block may compute and yield a
value like any other k2 block.

```k2
/// Build a 256-entry parity table at compile time. The loop, the mutation, and
/// the array all run in the compiler; only the finished table reaches the binary.
const parity: [256]u1 = comptime blk: {
    var table: [256]u1 = undefined;
    var i: usize = 0;
    while (i < 256) : (i += 1) {
        var bits: usize = i;
        var p: u1 = 0;
        while (bits != 0) : (bits >>= 1) {
            p ^= @intCast(bits & 1);
        }
        table[i] = p;
    }
    break :blk table;
};
```

The `var table` here is a **comptime var**: it is mutable while the compiler
evaluates the block, then frozen into the immutable `const parity`. No loop runs
at runtime; `parity` is a 256-byte constant in the program image, exactly as if
you had typed all 256 entries by hand. This is the *zero-cost abstraction*
requirement in action — the abstraction (a generating loop) is real in the
source and absent in the machine code.

### 3.3 Comptime `var` versus runtime `var`

A `var` declared inside a `comptime` block (or otherwise used only at compile
time) is a **comptime var** — it lives in the compiler's evaluation, can be
mutated freely during comptime, and never appears at runtime. A `var` in
ordinary code is a runtime variable. The distinction is, again, a property of
*context*, not a separate declaration form: the same `var name: T = ...;`
spelling is used for both, and the compiler classifies it by where it lives.

```k2
fn sumTo(comptime n: usize) usize {
    comptime {
        var total: usize = 0;        // comptime var: mutated by the compiler
        var i: usize = 1;
        while (i <= n) : (i += 1) total += i;
        return total;                // result folded into the call site
    }
}

const s = sumTo(100);                // == 5050, computed at compile time
```

---

## 4. `comptime` parameters and generics

A function parameter prefixed with `comptime` must be supplied a comptime-known
argument. The compiler then evaluates the function *with that argument fixed*,
specializing the function body. This single feature is the entirety of k2's
generics: there is no separate generics grammar, as established in
[§14 of Chapter 02](02-types.md#14-generics-are-comptime-functions-returning-type).

### 4.1 Comptime value parameters

The argument to a `comptime` parameter is known to the compiler, so it can be
used anywhere a comptime value is required — as an array length, a loop bound
that is unrolled, a `switch` over a known constant, and so on.

```k2
/// `n` is a comptime parameter, so it may size an array. Each distinct `n`
/// produces a distinct, monomorphized instantiation of `Buffer`.
fn Buffer(comptime n: usize) type {
    return struct {
        data: [n]u8,
        len: usize = 0,
    };
}

const Small = Buffer(64);    // a struct with a [64]u8 field
const Large = Buffer(4096);  // a distinct struct with a [4096]u8 field
```

`Buffer(64)` and `Buffer(4096)` are ordinary function calls evaluated at
compile time; each returns a `type` value. Because the argument is comptime, the
array length `[n]u8` is comptime-known, satisfying the type system's requirement
that array lengths be fixed.

### 4.2 Comptime `type` parameters: generics

When the comptime parameter has type `type`, the function is a **generic**. It
takes one or more types and (usually) returns a new type computed from them.

```k2
const std = @import("std");

/// A pair of two (possibly different) types. `Pair` is a function from two
/// types to a type — the whole of k2 generics, with no special syntax.
pub fn Pair(comptime A: type, comptime B: type) type {
    return struct {
        const Self = @This();

        first: A,
        second: B,

        pub fn init(a: A, b: B) Self {
            return Self{ .first = a, .second = b };
        }

        pub fn swap(self: Self) Pair(B, A) {
            return Pair(B, A).init(self.second, self.first);
        }
    };
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p = Pair(u32, []const u8).init(7, "seven");
    try out.print("{d} -> {s}\n", .{ p.first, p.second });
}
```

`@This()` (used as `const Self = @This();`) names the enclosing struct type from
inside its own body — indispensable because the generated struct is anonymous
until bound to the result. The pattern is exactly the one used by `List` in
[§14 of Chapter 02](02-types.md#14-generics-are-comptime-functions-returning-type).

### 4.3 Monomorphization and caching

Each distinct tuple of comptime arguments produces **one** instantiation, which
the compiler computes once and **caches**: `Pair(u32, bool)` evaluated in two
different files yields the *same* type, with the same identity, layout, and
methods. Two instantiations with different arguments — `Pair(u32, bool)` versus
`Pair(u32, u8)` — are distinct types and do not unify. This is **monomorphization**:
after comptime evaluation, the program contains a concrete, fully-specialized
copy of each used instantiation, and the generic "function" has vanished. There
is no runtime dispatch, no type erasure, and no boxing — the *native speed, no
runtime* pillar applied to generics.

### 4.4 Generic *functions* (not just generic types)

A `comptime type` parameter also makes an ordinary function polymorphic without
returning a type:

```k2
fn maxOf(comptime T: type, a: T, b: T) T {
    return if (a > b) a else b;
}

const m = maxOf(i32, 3, 9);     // T fixed to i32; specialized at the call site
```

The closely related `anytype` parameter infers its type from the argument via
`@TypeOf` at the call site, covering the case where you do not want to write the
type explicitly:

```k2
fn doubled(x: anytype) @TypeOf(x) {
    return x + x;               // works for any type supporting `+`
}
```

`comptime T: type` (explicit, named, reusable in the signature) and `anytype`
(inferred, anonymous) are the two spellings of generic functions, and together
with type-returning generics they span the whole design space — all from the one
rule that comptime parameters carry comptime-known values.

---

## 5. `type` is a first-class comptime value

The single fact that powers everything above is that **`type` is itself a
value** ([§2.1 of Chapter 02](02-types.md#21-type)). A value of type `type` *is*
a type. You can bind one to a `const`, store several in a comptime array, pass
them to functions, branch on them, compare them for equality, and return them.

### 5.1 Types as data

```k2
const Int = i32;                       // Int : type
const Maybe = ?Int;                    // build a new type from another
const Three = [3]Int;                  // and another

// A comptime array OF types — types are ordinary values.
const numeric = [_]type{ i8, i16, i32, i64, f32, f64 };

// Branch on a comptime flag to choose a type.
fn pick(comptime wide: bool) type {
    return if (wide) u64 else u32;
}
```

Type values support equality (`T == U`) and may be compared against the tag of
their `@typeInfo` (`@typeInfo(T) == .Int`), which is how generic code inspects
and constrains its type parameters (§7, §9).

### 5.2 `type` exists only at compile time

A value of type `type` can never appear at runtime: there is no `type` to store
in a runtime `var`, no way to read one from I/O, and no in-memory representation
for it in the running program. Types are consumed entirely during compilation —
to lay out structs, to select instantiations, to drive `inline for`. By the time
the binary runs, every `type` has been resolved into concrete layouts and code.
This is why `type`, `comptime_int`, and `comptime_float` are grouped together as
the **comptime-only** types: they are tools for the compiler, not data for the
CPU.

### 5.3 Functions over types are the only abstraction tool you need

Because types are values and functions can take and return them, the following
all reduce to "a function over types":

- **Generic containers** — `fn List(comptime T: type) type` (Chapter 02).
- **Generic algorithms** — `fn maxOf(comptime T: type, ...) T`.
- **Trait-style constraints** — a function that inspects `@typeInfo(T)` and
  `@compileError`s if `T` is unsuitable (§9).
- **Type transformers** — `fn Optionalize(comptime T: type) type`, returning
  `?T`; or the serializer of §11 that *derives* a wire layout from a struct.

One mechanism, used at every level of abstraction — the *one obvious way* pillar
realized as a single concept rather than a family of features.

---

## 6. Comptime control flow: `inline for` and `inline while`

Ordinary `for` and `while` run at runtime: one copy of the loop body, executed
repeatedly, with a runtime loop counter. Their **`inline`** variants are
comptime control flow: the compiler **unrolls** the loop, emitting the body once
per iteration with the loop variable bound to a distinct comptime-known value
each time.

This matters because each unrolled copy can use its loop variable as a
*comptime* value — to index a tuple of heterogeneous types, to access a
comptime-named field, or to instantiate a different generic per iteration. A
plain `for` cannot do this: its counter is runtime-known.

### 6.1 `inline for`

```k2
const std = @import("std");

/// Print every field of a struct. `inline for` unrolls over the comptime-known
/// field list, so `field.name` is comptime in each emitted copy and can drive
/// `@field`. A runtime `for` could not do this — its index isn't comptime.
fn printFields(comptime T: type, out: anytype, value: T) !void {
    const info = @typeInfo(T);
    inline for (info.Struct.fields) |field| {
        try out.print("{s} = {any}\n", .{ field.name, @field(value, field.name) });
    }
}
```

The compiler produces one `out.print(...)` call per field, with `field.name`
substituted as a comptime string in each. The loop itself does not exist at
runtime; only the unrolled calls do. This is the same construct used in
[§6.8 of Chapter 06](06-error-handling.md#68-comptime-introspection-of-error-sets)
to walk an error set's members.

### 6.2 `inline while`

`inline while` unrolls a `while` whose condition and step are comptime-known,
emitting the body once per iteration:

```k2
/// Sum the bit widths of a comptime list of integer types, unrolled at comptime.
fn totalBits(comptime types: []const type) comptime_int {
    comptime {
        var bits: comptime_int = 0;
        var i: usize = 0;
        inline while (i < types.len) : (i += 1) {
            bits += @typeInfo(types[i]).Int.bits;
        }
        return bits;
    }
}

const width = totalBits(&.{ u8, u16, u32 });   // == 56, computed at compile time
```

### 6.3 When to use the `inline` variants

- Use `inline for` / `inline while` when the loop body must use the loop value
  **as a comptime value** — indexing heterogeneous data (a tuple of mixed
  types), naming a field/variant via `@field`, or instantiating a generic per
  iteration.
- Use a plain `for` / `while` for ordinary runtime iteration over data. Do not
  reach for `inline` merely for speed: an `inline` loop over a large
  comptime-known count produces a large amount of code (one copy per iteration).
  It is a *metaprogramming* tool, not a manual optimizer.

The boundary discipline of §2 still holds: an `inline` loop requires its bound
to be comptime-known, and the compiler rejects an `inline for` over a
runtime-length slice with a precise diagnostic.

---

## 7. Reflection: `@typeInfo`, `@TypeOf`, `@hasField`, `@field`

k2's type system is **reified**: a type can be deconstructed into structured
data, inspected with ordinary k2, and (via `@Type`, §8) reconstructed. The four
builtins in this section are the inspection half of that round-trip. All of them
run at compile time and emit no runtime code.

### 7.1 `@TypeOf`

`@TypeOf(expr, ...)` returns the *type* of one or more expressions without
evaluating them at runtime. It is the basis for type inference in generic code —
most often to name the return type of an `anytype` function.

```k2
const x: u64 = 9;
const T = @TypeOf(x);          // T == u64, a comptime value of type `type`

fn addOne(v: anytype) @TypeOf(v) {
    return v + 1;              // return type follows the argument's type
}
```

When given several arguments, `@TypeOf` returns their common *peer type* — the
single type all of them coerce to — which is how generic code finds the result
type of a mixed expression.

### 7.2 `@typeInfo`

`@typeInfo(T)` reflects on a type, yielding a tagged-union value describing it.
The active tag identifies the kind of type, and the payload carries its
structure. The principal variants:

| Tag           | Payload (selected fields) |
| ------------- | ------------------------- |
| `.Int`        | `signedness`, `bits` |
| `.Float`      | `bits` |
| `.Bool`       | (none) |
| `.Pointer`    | `size` (`.One`/`.Slice`), `is_const`, `child: type` |
| `.Array`      | `len`, `child: type` |
| `.Optional`   | `child: type` |
| `.ErrorUnion` | `error_set: type`, `payload: type` |
| `.ErrorSet`   | a comptime list of members, each with a `.name` |
| `.Struct`     | `fields` (each `.name`, `.type`, `.default_value`), `layout` |
| `.Enum`       | `tag_type: type`, `fields` (each `.name`, `.value`) |
| `.Union`      | `tag_type: ?type`, `fields` (each `.name`, `.type`) |
| `.Void`, `.Type`, `.NoReturn` | (none) |

You consume it by comparing the active tag and reading the payload. Because the
result is an ordinary tagged union, you pattern-match it with the same `switch`
and field access used everywhere else in k2:

```k2
fn elementCount(comptime T: type) usize {
    const info = @typeInfo(T);
    return switch (info) {
        .Struct => |s| s.fields.len,
        .Enum => |e| e.fields.len,
        .Array => |a| a.len,
        else => 0,
    };
}
```

The tag-comparison form `info == .Struct` (and `info != .Struct`) is the
idiomatic guard, exactly as used in the canonical `printFields` snippet and in
[§14 of Chapter 02](02-types.md#14-generics-are-comptime-functions-returning-type)'s
`Vector`.

### 7.3 `@hasField`

`@hasField(T, name)` is a comptime predicate: it is `true` when type `T` has a
field (struct field, enum variant, or union variant) named by the comptime
string `name`. It is the convenient shorthand for "does this type have this
member?" without manually walking `@typeInfo`.

```k2
fn hasTimestamp(comptime T: type) bool {
    return @hasField(T, "timestamp");
}

const Event = struct { id: u32, timestamp: u64 };
const tagged = hasTimestamp(Event);    // true, decided at compile time
```

This is the primary tool for *structural*, trait-style checks: a generic can ask
whether a type supplies the fields it needs and either adapt or reject the type
with `@compileError` (§9).

### 7.4 `@field`

`@field(value, name)` accesses the field of `value` named by the comptime string
`name`. It is the dynamic-looking counterpart to `value.field` — except the
"dynamism" is entirely at compile time: `name` must be comptime-known, so each
`@field` expression resolves to a fixed, ordinary field access with zero runtime
overhead.

```k2
const std = @import("std");

const Point = struct { x: i32, y: i32 };

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p = Point{ .x = 3, .y = 4 };

    // Equivalent to p.x and p.y, but the name comes from a comptime string,
    // which lets it be driven by an `inline for` over the field list.
    const names = [_][]const u8{ "x", "y" };
    inline for (names) |name| {
        try out.print("{s} = {d}\n", .{ name, @field(p, name) });
    }
}
```

`@field` over a comptime-known struct *type* (rather than a value) recovers a
declaration — `@field(E, member_name)` recovers an error value from an error set
(as in [§6.8 of Chapter 06](06-error-handling.md#68-comptime-introspection-of-error-sets)),
and `@field(Enum, variant_name)` recovers an enum value. Together with
`@typeInfo`'s field/variant lists and `inline for`, `@field` is what lets a
metaprogram *generate code that touches every member of a type by name*, which is
the core move of the serializer in §11.

---

## 8. Synthesizing types with `@Type`

`@Type` is the inverse of `@typeInfo`: given a `@typeInfo`-shaped description, it
**constructs the corresponding type**. This closes the reflection round-trip —
`@typeInfo` takes a type apart into data, you transform that data with ordinary
k2, and `@Type` builds a new type from the result. Serializers, ABI shims, and
trait-derived wrappers are written as plain k2 that runs during compilation.

### 8.1 The round-trip

```k2
fn identity(comptime T: type) type {
    return @Type(@typeInfo(T));    // deconstruct then reconstruct: same type back
}

const Again = identity(u32);       // == u32
```

`@Type(@typeInfo(T))` is `T` for every `T`. The value in the round-trip is the
*transformation* you insert in the middle.

### 8.2 Building an integer type from a width

```k2
/// Construct an unsigned integer type of an arbitrary comptime bit width.
fn UInt(comptime bits: u16) type {
    return @Type(.{ .Int = .{ .signedness = .unsigned, .bits = bits } });
}

const U24 = UInt(24);              // a 24-bit unsigned integer type
const U7 = UInt(7);
```

The argument `.{ .Int = .{ ... } }` is an anonymous value of the `@typeInfo`
tagged union, with the `.Int` variant active. `@Type` reads that description and
yields the type it denotes.

### 8.3 Building a struct type from a field list

The most powerful use of `@Type` is generating *aggregate* types from a computed
field list — for example, projecting a struct down to a subset of its fields, or
deriving a parallel "columnar" layout. The following maps every field of a
struct to a new struct in which each field has been wrapped in an optional `?T`:

```k2
const std = @import("std");

/// Given `struct { a: A, b: B }`, produce `struct { a: ?A, b: ?B }`.
/// A patch/partial-update type derived from a record type, at comptime.
fn Partial(comptime T: type) type {
    const info = @typeInfo(T);
    if (info != .Struct) {
        @compileError("Partial requires a struct, got " ++ @typeName(T));
    }

    comptime {
        const src = info.Struct.fields;
        var fields: [src.len]StructField = undefined;

        inline for (src, 0..) |f, i| {
            fields[i] = .{
                .name = f.name,
                .type = ?f.type,          // wrap each field type in an optional
                .default_value = null,
                .alignment = @alignOf(?f.type),
            };
        }

        return @Type(.{ .Struct = .{
            .layout = .Auto,
            .fields = &fields,
            .decls = &.{},
            .is_tuple = false,
        } });
    }
}

const User = struct { id: u32, name: []const u8, age: u8 };
const UserPatch = Partial(User);   // struct { id: ?u32, name: ?[]const u8, age: ?u8 }
```

Here `StructField` is the field-descriptor type used by `@typeInfo(...).Struct.fields`
(provided by the standard reflection namespace). The transformation is ordinary
k2 — a loop building an array — and `@Type` turns the finished description into a
real, usable struct type. `UserPatch` is indistinguishable from a hand-written
`struct { id: ?u32, name: ?[]const u8, age: ?u8 }`; it has the same layout and the
same zero runtime cost.

### 8.4 Why the round-trip is the whole metaprogramming story

`@typeInfo` (inspect) + `@Type` (construct), with `inline for` to walk members
and `@field` to touch them by name, is a *complete* type-level metaprogramming
kit. There is nothing a macro system would add: you already have the type's full
structure as data, the full language to transform it, and a way to turn the
result back into a type. The two worked examples (§11, §12) are built entirely
from these pieces.

---

## 9. Comptime asserts and `@compileError`

Metaprograms must reject bad inputs. Because k2 metaprograms run at compile time,
they reject them with **compile errors**, not runtime failures — a generic given
an unsuitable type should not produce a subtly-broken program, it should refuse
to compile, with a precise, source-located message.

### 9.1 `@compileError`

`@compileError(message)` aborts compilation with `message`, located at the point
the comptime evaluator reaches it. It has type `noreturn` (see
[§2.3 of Chapter 02](02-types.md#23-noreturn)), so it satisfies any branch and
can stand in for a value the function would otherwise return:

```k2
/// Require a numeric element type; reject anything else at compile time, with a
/// message that names the offending type.
fn Vector(comptime T: type, comptime n: usize) type {
    const info = @typeInfo(T);
    if (info != .Int and info != .Float) {
        @compileError("Vector requires an integer or float element, got " ++ @typeName(T));
    }
    return struct {
        data: [n]T,
    };
}

// const bad = Vector(bool, 3);
// -> compile error: "Vector requires an integer or float element, got bool"
```

`@compileError` is reached only when the comptime evaluator *executes* that path.
A `@compileError` inside an `if` that the compiler determines is never taken for
a given instantiation does not fire — it is ordinary control flow, evaluated at
comptime. This is what makes it a precise constraint mechanism: you guard the bad
case and abort exactly there.

### 9.2 Comptime asserts

A comptime assertion is just an `if` that ends in `@compileError`. The standard
library provides a `comptime`-friendly `assert`, but the primitive is this
pattern:

```k2
fn assertPowerOfTwo(comptime n: usize) void {
    if (n == 0 or (n & (n - 1)) != 0) {
        @compileError("expected a power of two");
    }
}

fn RingBuffer(comptime T: type, comptime cap: usize) type {
    comptime assertPowerOfTwo(cap);    // checked when this instantiation is built
    return struct {
        data: [cap]T,
        head: usize = 0,
        tail: usize = 0,
        // ... mask arithmetic relies on `cap` being a power of two
    };
}
```

Because the check runs while the type is being instantiated, `RingBuffer(u8, 100)`
fails to compile (100 is not a power of two) and names the violated requirement,
while `RingBuffer(u8, 128)` succeeds. The invalid program never reaches a runtime
where the mask arithmetic would silently misbehave — a class of bug is moved from
runtime to compile time.

### 9.3 `@compileLog` for debugging metaprograms

`@compileLog(values...)` prints comptime values *during compilation* without
halting it. It is the metaprogrammer's `print`-debugging: drop it inside a
comptime block to see what the compiler is computing, then remove it. A program
that still contains a `@compileLog` is treated as not-yet-finished and the
compiler reports the logged values, to keep stray debug logging from shipping.

```k2
fn debugInfo(comptime T: type) void {
    @compileLog("typeInfo of", @typeName(T), @typeInfo(T));
}
```

### 9.4 Diagnostics are part of the API

Because constraints are expressed in ordinary comptime code, the *quality of the
error message* is under your control: include `@typeName(T)` to name the bad
type, state the requirement, and the user sees a message as good as any built-in
diagnostic — at the instantiation site, before the program runs. This is the
direct comptime analogue of the *errors are values* discipline: a generic's
preconditions are checked and reported explicitly, never deferred to a runtime
surprise.

---

## 10. Evaluation rules, sandboxing, and termination

Comptime is ordinary k2 run by the compiler — but "run by the compiler" imposes
rules that runtime code does not face. This section states them normatively.

### 10.1 What comptime code may do

Comptime evaluation may:

- perform any pure computation: arithmetic, control flow, recursion, function
  calls, struct/array construction and (comptime-`var`) mutation;
- compute with `type` values, call `@typeInfo`/`@Type`/`@TypeOf`/`@hasField`/
  `@field`/`@sizeOf`/`@alignOf`, and instantiate generics;
- read comptime-known data: literals, `const`s, `@embedFile` bytes, the fields
  and members reported by reflection;
- abort with `@compileError` or log with `@compileLog`.

### 10.2 What comptime code may *not* do

Comptime evaluation runs in a **sandbox**. It may **not**:

- **Perform I/O or touch the outside world.** There is no `sys: *System` at
  comptime — the root capability is a *runtime* value handed to `main`. The
  *no ambient authority* pillar (see the capability model in
  [Chapter 02](02-types.md) and the charter) means there is simply no ambient
  way to read a file, the clock, the network, or the environment, and comptime
  has no capability handle either. The one compile-time way to pull external
  bytes into the program is `@embedFile`, which embeds a file's contents as a
  comptime-known array — a deliberate, visible, allocation-free exception.
- **Allocate from a runtime allocator.** Heap allocation requires an `Allocator`
  capability, a runtime value; comptime has none. Comptime data (arrays, built-up
  field lists, generated tables) lives in the compiler's own memory and is
  materialized into the binary as constants, not via `alloc`. This is the
  *no hidden allocation* pillar reaching into compile time: a comptime metaprogram
  cannot secretly allocate any more than runtime code can.
- **Observe runtime values.** Per §2, runtime values never flow back into
  comptime. A comptime context that requires a value the compiler does not have
  is a compile error.

### 10.3 Termination

Comptime evaluation is **required to terminate**. A nonterminating comptime
computation is not a hang in your program — it is the *compiler* failing to make
progress, which is unacceptable. k2 enforces termination with a **comptime
evaluation budget**: a bound (a "branch quota") on the number of backward
branches and calls a single comptime evaluation may perform. Exceeding it is a
compile error reported at the offending construct, not an infinite compile.

```k2
// A metaprogram that recurses without a comptime-known base case will exhaust
// the evaluation budget and produce a compile-time diagnostic — never an
// infinite compile and never a runtime hang.
```

The budget is generous and adjustable (a metaprogram that legitimately needs more
iterations can raise it with the standard `@setEvalBranchQuota`-style facility),
but its *existence* is the guarantee: the compiler always either finishes the
metaprogram or tells you precisely why it stopped.

### 10.4 Determinism

Because comptime code cannot observe I/O, the clock, randomness, or any ambient
state, comptime evaluation is **deterministic**: the same inputs produce the same
types, constants, and tables on every build, on every machine, for every target.
This is what makes monomorphization cacheable (§4.3) and builds reproducible — a
direct dividend of the capability model. The serializer of §11 and the hash map
of §12 generate *byte-for-byte identical* code on every compilation, because the
metaprograms that produce them are pure functions of their type arguments.

### 10.5 Where comptime sits in the pipeline

Per the compiler pipeline, comptime evaluation runs after type inference and
before monomorphization: a sandboxed interpreter executes all comptime code,
instantiates every used generic, evaluates the reflection builtins, and resolves
`@compileError` diagnostics; the result is a fully concrete, generics-expanded
IR with all comptime values folded. By the time either backend (Cranelift for
Debug, LLVM for Release) runs, no `type` values, no `comptime_int`s, and no
generic functions remain — only concrete code and baked-in constants. That is why
comptime abstractions are *zero-cost*: they are entirely consumed before code
generation begins.

---

## 11. Worked example: a comptime-generated serializer

This example derives a compact binary serializer **from a type's structure**,
using only the reflection tools of §7 and the comptime control flow of §6. No
macros, no code generator, no runtime reflection — the encoder and decoder for a
given type are generated when the program is compiled, and run at full native
speed.

### 11.1 Goal and format

We want `Serializer(T)` for any struct `T` whose fields are fixed-width integers
and arrays/slices of bytes, producing two functions:

- `encode(value: T, out: []u8) usize` — write `value` into `out`, returning the
  number of bytes written.
- `decode(in: []const u8) DecodeError!T` — read a `T` back out of `in`.

The wire format is the obvious one: each field is written in declaration order,
integers little-endian in their natural width, byte slices as a `u32` length
prefix followed by the bytes. The whole format is *derived from* `@typeInfo(T)`,
so adding a field to the struct automatically updates both directions.

### 11.2 The generator

```k2
const std = @import("std");

const DecodeError = error{ ShortBuffer, BadLength };

/// Generate a binary serializer for `T` at compile time, by reflecting on its
/// fields. `Serializer(T)` is a function from a type to a type — the encoder and
/// decoder are specialized to `T`'s exact layout, with no runtime reflection.
pub fn Serializer(comptime T: type) type {
    const info = @typeInfo(T);
    if (info != .Struct) {
        @compileError("Serializer requires a struct, got " ++ @typeName(T));
    }

    return struct {
        const Self = @This();
        const fields = info.Struct.fields;

        /// Write `value` into `out`. Returns the number of bytes written.
        /// The body is unrolled once per field at comptime.
        pub fn encode(value: T, out: []u8) usize {
            var pos: usize = 0;
            inline for (fields) |field| {
                const fv = @field(value, field.name);
                pos += writeOne(@TypeOf(fv), fv, out[pos..]);
            }
            return pos;
        }

        /// Read a `T` back out of `in`. Field types drive the decoding; a too-short
        /// buffer is a recoverable error value, not a panic.
        pub fn decode(in: []const u8) DecodeError!T {
            var pos: usize = 0;
            var result: T = undefined;
            inline for (fields) |field| {
                const FieldT = field.type;
                const r = try readOne(FieldT, in[pos..]);
                @field(result, field.name) = r.value;
                pos += r.size;
            }
            return result;
        }

        // ---- per-field codecs, selected by type at comptime ----

        fn writeOne(comptime F: type, v: F, out: []u8) usize {
            const fi = @typeInfo(F);
            switch (fi) {
                .Int => {
                    const n = @sizeOf(F);
                    var i: usize = 0;
                    var bits = v;
                    // little-endian: low byte first
                    inline while (i < n) : (i += 1) {
                        out[i] = @intCast(@as(u64, @as(UnsignedOf(F), @bitCast(bits))) & 0xFF);
                        bits >>= 8;
                    }
                    return n;
                },
                .Pointer => |p| {
                    // []const u8 byte slices: u32 length prefix, then the bytes.
                    if (p.size != .Slice or p.child != u8) {
                        @compileError("Serializer only supports []const u8 slices, got " ++ @typeName(F));
                    }
                    const len: u32 = @intCast(v.len);
                    _ = writeOne(u32, len, out[0..]);
                    for (v, 0..) |b, j| out[4 + j] = b;
                    return 4 + v.len;
                },
                else => @compileError("Serializer cannot encode field of type " ++ @typeName(F)),
            }
        }

        const Read = struct { value: anytype, size: usize };

        fn readOne(comptime F: type, in: []const u8) DecodeError!ReadResult(F) {
            const fi = @typeInfo(F);
            switch (fi) {
                .Int => {
                    const n = @sizeOf(F);
                    if (in.len < n) return DecodeError.ShortBuffer;
                    var acc: UnsignedOf(F) = 0;
                    var i: usize = 0;
                    inline while (i < n) : (i += 1) {
                        acc |= @as(UnsignedOf(F), in[i]) << @intCast(i * 8);
                    }
                    return .{ .value = @bitCast(acc), .size = n };
                },
                .Pointer => |p| {
                    if (p.size != .Slice or p.child != u8) {
                        @compileError("Serializer only supports []const u8 slices, got " ++ @typeName(F));
                    }
                    if (in.len < 4) return DecodeError.ShortBuffer;
                    const lr = try readOne(u32, in[0..]);
                    const len: usize = lr.value;
                    if (in.len < 4 + len) return DecodeError.ShortBuffer;
                    return .{ .value = in[4 .. 4 + len], .size = 4 + len };
                },
                else => @compileError("Serializer cannot decode field of type " ++ @typeName(F)),
            }
        }
    };
}

/// The unsigned integer type with the same bit width as integer type `F`,
/// synthesized at comptime so bit operations are well-defined regardless of
/// `F`'s signedness.
fn UnsignedOf(comptime F: type) type {
    return @Type(.{ .Int = .{ .signedness = .unsigned, .bits = @typeInfo(F).Int.bits } });
}

/// The result type of decoding a field of type `F`: the value plus its byte size.
fn ReadResult(comptime F: type) type {
    return struct { value: F, size: usize };
}
```

### 11.3 Using it

```k2
const std = @import("std");

const Message = struct {
    id: u32,
    code: i16,
    body: []const u8,
};

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const alloc = sys.heap;

    const Codec = Serializer(Message);

    const msg = Message{ .id = 0xCAFE, .code = -3, .body = "hello" };

    // A buffer large enough to hold the encoding; sized by hand, no hidden alloc.
    const buf: []u8 = try alloc.alloc(u8, 64);
    defer alloc.free(buf);

    const written = Codec.encode(msg, buf);
    try out.print("encoded {d} bytes\n", .{written});

    // Round-trip back. A short buffer would be a DecodeError, handled explicitly.
    const got = Codec.decode(buf[0..written]) catch |err| {
        try out.print("decode failed: {s}\n", .{@errorName(err)});
        return;
    };

    try out.print("id={d} code={d} body={s}\n", .{ got.id, got.code, got.body });
}
```

### 11.4 What the metaprogram bought us

- **The format is derived, not hand-maintained.** Add a `flags: u8` field to
  `Message` and both `encode` and `decode` gain it automatically, in declaration
  order — there is no separate schema to keep in sync. The single source of truth
  is the struct.
- **It is fully specialized.** `Serializer(Message).encode` is monomorphized to
  `Message`'s exact fields; the `inline for` over `fields` is unrolled, the
  per-field type switch is resolved at comptime, and the emitted machine code is a
  straight-line sequence of byte writes — the same code you would write by hand,
  per the *native speed, no runtime* pillar.
- **Errors are values, mistakes are panics.** A truncated input buffer is an
  *expected, recoverable* condition, so `decode` returns `DecodeError.ShortBuffer`
  for the caller to `catch` ([Chapter 06](06-error-handling.md)). An *unsupported
  field type* is a *programmer mistake* in the metaprogram, caught at compile time
  by `@compileError` (§9) — the distinction Chapter 06 §7.3 insists on, applied to
  generated code.
- **No hidden allocation.** The serializer writes into a caller-provided `[]u8`;
  the only allocation in the example is the explicit `alloc.alloc` in `main`,
  paired with `defer alloc.free`. The generator itself allocates nothing — its
  field walking happens in the compiler.

---

## 12. Worked example: a generic open-addressing hash map

This example is a complete `HashMap(K, V)` built as a comptime function returning
a type. It uses open addressing with linear probing and a load-factor-driven
resize, threads the `Allocator` capability explicitly, and derives its hashing
and equality from the key type via reflection — demonstrating generics,
`comptime` constraints, capability passing, and `errdefer` together.

### 12.1 Design

- `HashMap(K, V)` is `fn HashMap(comptime K: type, comptime V: type) type`.
- Storage is a single flat slice of `Slot` (key, value, and a 1-byte state:
  empty / used / tombstone), allocated through the `Allocator` capability.
- Collisions are resolved by **linear probing**; deletions leave **tombstones**
  so probe sequences are not broken.
- The map grows (capacity doubles, always a power of two) when the load factor
  exceeds 7/8, rehashing every live entry.
- Hashing and equality are obtained from the key type: an integer key hashes by
  bit-mixing; a `[]const u8` key hashes byte-by-byte. The choice is made at
  compile time by reflecting on `K`, and an unsupported `K` is rejected with
  `@compileError`.

### 12.2 The generator

```k2
const std = @import("std");

pub fn HashMap(comptime K: type, comptime V: type) type {
    // Reject key types we cannot hash, with a precise diagnostic (§9).
    comptime validateKey(K);

    return struct {
        const Self = @This();

        const State = enum(u8) { empty, used, tombstone };

        const Slot = struct {
            key: K,
            value: V,
            state: State = .empty,
        };

        slots: []Slot,
        count: usize,        // live entries
        used: usize,         // live + tombstone entries (occupied probe positions)
        alloc: Allocator,

        /// Construct an empty map. No allocation happens until the first insert,
        /// keeping with "no hidden allocation": the empty map owns no heap memory.
        pub fn init(alloc: Allocator) Self {
            return Self{ .slots = &.{}, .count = 0, .used = 0, .alloc = alloc };
        }

        /// Release the backing store. Paired with `init` via `defer` at the call site.
        pub fn deinit(self: *Self) void {
            if (self.slots.len != 0) self.alloc.free(self.slots);
            self.slots = &.{};
            self.count = 0;
            self.used = 0;
        }

        /// Insert or overwrite. Returns true if a new key was added.
        pub fn put(self: *Self, key: K, value: V) !bool {
            // Grow before the load factor passes 7/8 (using `used`, which counts
            // tombstones, so probe chains stay short).
            if (self.slots.len == 0 or (self.used + 1) * 8 >= self.slots.len * 7) {
                try self.grow();
            }

            const mask = self.slots.len - 1;
            var i: usize = hashKey(key) & mask;
            var first_tombstone: ?usize = null;

            while (true) : (i = (i + 1) & mask) {
                const slot = &self.slots[i];
                switch (slot.state) {
                    .empty => {
                        // Reuse an earlier tombstone if we passed one.
                        const target = first_tombstone orelse i;
                        self.slots[target] = .{ .key = key, .value = value, .state = .used };
                        self.count += 1;
                        if (first_tombstone == null) self.used += 1;
                        return true;
                    },
                    .tombstone => {
                        if (first_tombstone == null) first_tombstone = i;
                    },
                    .used => {
                        if (eql(slot.key, key)) {
                            slot.value = value;   // overwrite existing key
                            return false;
                        }
                    },
                }
            }
        }

        /// Look up a key. Absence is modeled with an optional `?V` (Chapter 02 §6),
        /// not a sentinel value.
        pub fn get(self: *const Self, key: K) ?V {
            if (self.slots.len == 0) return null;
            const mask = self.slots.len - 1;
            var i: usize = hashKey(key) & mask;
            while (true) : (i = (i + 1) & mask) {
                const slot = &self.slots[i];
                switch (slot.state) {
                    .empty => return null,                // end of probe chain
                    .tombstone => {},                     // skip; keep probing
                    .used => if (eql(slot.key, key)) return slot.value,
                }
            }
        }

        /// Remove a key. Returns true if it was present. Leaves a tombstone so
        /// that probe sequences through this position are preserved.
        pub fn remove(self: *Self, key: K) bool {
            if (self.slots.len == 0) return false;
            const mask = self.slots.len - 1;
            var i: usize = hashKey(key) & mask;
            while (true) : (i = (i + 1) & mask) {
                const slot = &self.slots[i];
                switch (slot.state) {
                    .empty => return false,
                    .tombstone => {},
                    .used => if (eql(slot.key, key)) {
                        slot.state = .tombstone;
                        self.count -= 1;
                        return true;
                    },
                }
            }
        }

        pub fn len(self: *const Self) usize {
            return self.count;
        }

        // ---- internal: growth and rehashing ----

        fn grow(self: *Self) !void {
            const new_cap = if (self.slots.len == 0) 16 else self.slots.len * 2;

            const fresh: []Slot = try self.alloc.alloc(Slot, new_cap);
            // If a later step failed we would own a half-built buffer; free it.
            errdefer self.alloc.free(fresh);

            for (fresh) |*s| s.* = .{ .key = undefined, .value = undefined, .state = .empty };

            const old = self.slots;
            self.slots = fresh;
            self.count = 0;
            self.used = 0;

            // Reinsert every live entry into the new table (drops tombstones).
            for (old) |slot| {
                if (slot.state == .used) {
                    _ = self.putNoGrow(slot.key, slot.value);
                }
            }

            if (old.len != 0) self.alloc.free(old);
        }

        /// Insert assuming there is room (used only during `grow`'s rehash).
        fn putNoGrow(self: *Self, key: K, value: V) bool {
            const mask = self.slots.len - 1;
            var i: usize = hashKey(key) & mask;
            while (true) : (i = (i + 1) & mask) {
                if (self.slots[i].state != .used) {
                    self.slots[i] = .{ .key = key, .value = value, .state = .used };
                    self.count += 1;
                    self.used += 1;
                    return true;
                }
            }
        }

        // ---- hashing and equality, selected from K at comptime ----

        fn hashKey(key: K) usize {
            const info = @typeInfo(K);
            if (info == .Int) {
                // Bit-mix an integer key (a small splittable-hash style finalizer).
                var x: u64 = @intCast(@as(UnsignedOf(K), @bitCast(key)));
                x ^= x >> 33;
                x = std.math.wrapMul(u64, x, 0xff51afd7ed558ccd);
                x ^= x >> 33;
                return @intCast(x);
            } else {
                // []const u8 key: FNV-1a over the bytes (validated in validateKey).
                var h: u64 = 0xcbf29ce484222325;
                for (key) |b| {
                    h ^= b;
                    h = std.math.wrapMul(u64, h, 0x100000001b3);
                }
                return @intCast(h);
            }
        }

        fn eql(a: K, b: K) bool {
            const info = @typeInfo(K);
            if (info == .Int) {
                return a == b;
            } else {
                // []const u8 comparison
                if (a.len != b.len) return false;
                for (a, b) |x, y| {
                    if (x != y) return false;
                }
                return true;
            }
        }
    };
}

/// Compile-time key-type constraint: integers and byte slices are supported.
fn validateKey(comptime K: type) void {
    const info = @typeInfo(K);
    if (info == .Int) return;
    if (info == .Pointer and info.Pointer.size == .Slice and info.Pointer.child == u8) return;
    @compileError("HashMap key must be an integer or []const u8, got " ++ @typeName(K));
}

fn UnsignedOf(comptime F: type) type {
    return @Type(.{ .Int = .{ .signedness = .unsigned, .bits = @typeInfo(F).Int.bits } });
}
```

### 12.3 Using it

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // The heap is a capability; the map never allocates without it.
    var counts = HashMap([]const u8, u32).init(sys.heap);
    defer counts.deinit();        // explicit cleanup, no implicit destructor

    const words = [_][]const u8{ "k2", "zig", "k2", "rust", "k2", "zig" };
    for (words) |w| {
        const current = counts.get(w) orelse 0;     // ?u32 -> default via orelse
        _ = try counts.put(w, current + 1);
    }

    try out.print("distinct words: {d}\n", .{counts.len()});
    try out.print("k2   -> {d}\n", .{counts.get("k2") orelse 0});
    try out.print("zig  -> {d}\n", .{counts.get("zig") orelse 0});
    try out.print("rust -> {d}\n", .{counts.get("rust") orelse 0});

    _ = counts.remove("rust");
    try out.print("after remove, distinct: {d}\n", .{counts.len()});

    // An integer-keyed map is a *distinct* monomorphization of the same generic.
    var squares = HashMap(u32, u64).init(sys.heap);
    defer squares.deinit();
    var i: u32 = 1;
    while (i <= 5) : (i += 1) {
        _ = try squares.put(i, @as(u64, i) * i);
    }
    try out.print("4*4 = {d}\n", .{squares.get(4) orelse 0});
}
```

### 12.4 What this example demonstrates

- **Generics as functions.** `HashMap` is `fn HashMap(comptime K: type, comptime V: type) type`.
  `HashMap([]const u8, u32)` and `HashMap(u32, u64)` are two *distinct*,
  independently monomorphized types (§4.3) — no shared runtime machinery, no type
  erasure.
- **Comptime constraints and reflection.** `validateKey` rejects unsupported key
  types at compile time with a named diagnostic (§9), and `hashKey`/`eql` select
  their strategy by reflecting on `K` with `@typeInfo` (§7). The `if (info == .Int)`
  branch is resolved at comptime per instantiation, so the *integer* map contains
  only the integer hash code and the *slice* map only the FNV-1a code — the dead
  branch is folded away (§10.5), not carried at runtime.
- **`@Type` in a real role.** `UnsignedOf(K)` synthesizes the same-width unsigned
  type so the bit-mixing finalizer is well-defined for signed and unsigned integer
  keys alike (§8.2).
- **The capability model, threaded explicitly.** The `Allocator` is a field,
  passed into `init` and used for every `alloc`/`free`. The empty map owns no heap
  memory (`init` allocates nothing); growth is the only allocation site, and it is
  visible. `deinit` is paired with `defer` at every call site — *no hidden
  allocation*, *no ambient authority*, cleanup written explicitly.
- **`errdefer` for a half-built resource.** In `grow`, the freshly allocated
  buffer is guarded by `errdefer self.alloc.free(fresh)` so that if any later step
  in that scope failed, the new buffer is released rather than leaked — the exact
  idiom from [§6.5 of Chapter 06](06-error-handling.md#65-errdefer-cleanup-on-the-error-path).
- **Errors are values; absence is an optional.** `put` returns `!bool` (it can
  fail to allocate); `get` returns `?V` and the caller supplies a default with
  `orelse` ([Chapter 02 §6](02-types.md#6-optionals-t)). The two concepts —
  *failure* and *absence* — stay distinct, as the type system intends.

---

## 13. Summary

- **comptime is ordinary k2 run by the compiler.** It is the single
  metaprogramming mechanism: no macros, no templates, no separate generics
  grammar. You learn it once and it covers generics, reflection, constants, and
  code generation.
- **The comptime/runtime boundary is precise and visible.** Comptime-known
  values (literals, `const`s, `type`/`comptime_int`/`comptime_float`, and the
  results of `@`-builtins and comptime calls) flow *down* into runtime as baked
  constants; runtime values never flow *back* — that is a compile error, not a
  silent migration.
- **`comptime` forces evaluation.** On an expression (`comptime expr`), on a
  block (`comptime { ... }` with comptime `var`s the compiler mutates and then
  freezes), and on a parameter (`comptime x: T`) that must receive a
  comptime-known argument.
- **Generics are comptime functions over types.** `fn Name(comptime T: type) type`
  computes and returns a type; each distinct argument tuple is monomorphized once
  and cached, with no runtime dispatch. `@This()` names the generated type;
  `anytype` is the inferred-parameter spelling.
- **`type` is a first-class comptime value** — bindable, passable, comparable,
  returnable — and exists only at compile time. This single fact subsumes every
  abstraction tool the language offers.
- **`inline for` / `inline while`** are comptime control flow: the compiler
  unrolls them, binding the loop value as *comptime* in each emitted copy, which
  is what lets a loop drive `@field`, index heterogeneous data, and instantiate
  generics per iteration.
- **Reflection is reified.** `@TypeOf`, `@typeInfo`, `@hasField`, and `@field`
  deconstruct and inspect types and values at comptime; `@Type` reconstructs
  types from a `@typeInfo` description. The `@typeInfo` → transform → `@Type`
  round-trip is a complete type-level metaprogramming kit.
- **Bad instantiations are compile errors.** `@compileError` aborts with a
  precise, source-located message, and a comptime assert is just an `if` ending in
  `@compileError`; `@compileLog` prints comptime values while debugging a
  metaprogram.
- **Comptime is sandboxed, terminating, and deterministic.** It cannot perform
  I/O (no `*System` exists at comptime; `@embedFile` is the one visible exception),
  cannot allocate from a runtime allocator, must terminate within an evaluation
  budget, and is a pure function of its inputs — so builds are reproducible and
  every comptime abstraction is fully consumed before code generation.

The two worked examples close the loop: a binary serializer **derived** from a
struct's `@typeInfo`, and a generic `HashMap(K, V)` that **selects** hashing and
equality from its key type by reflection — both written as ordinary k2 that runs
at compile time, both compiling to exactly the hand-written-equivalent machine
code. That is the *Kardashev Type II* promise applied to metaprogramming: total
control over what the machine does, computed deliberately in the compiler, with
zero waste at runtime.

---

### See also

- **[Chapter 02 — The Type System](02-types.md):** `type` as a value, the
  composite type forms, and generics as comptime functions returning `type`
  (§14, §15).
- **[Chapter 04 — Functions](04-functions.md):** `comptime` parameters,
  `anytype`, and the function/method machinery generics are built on.
- **[Chapter 06 — Error Handling](06-error-handling.md):** errors as values,
  `try`/`catch`/`errdefer`, and comptime introspection of error sets (§6.8) — the
  same reflection used here for types.
