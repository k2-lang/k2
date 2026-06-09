# 02 — The Type System

> Part of the **k2** language specification.
> k2 stands for *Kardashev Type II* — total control over the machine, with zero waste.

This chapter specifies k2's type system: the primitive types, the
composite type constructors (pointers, slices, arrays, optionals, error
unions), the aggregate types (`struct`, `enum`, `union`), foreign-handle
modeling via `*anyopaque`, the rules for type coercion and
integer-overflow behavior, and the way generics fall out of the language
as ordinary compile-time functions that return `type`.

k2 is **statically and strongly typed**. Every value has a type known by
the time monomorphization runs, and there are no implicit conversions that
call user code or lose information silently. Types are written in
**postfix-modifier form** to the right of names, left-to-right, exactly as
introduced in the syntax summary: `?T`, `!T`, `E!T`, `*T`, `[]T`, `[N]T`.

A central, load-bearing idea of k2 is that **`type` is itself a value**.
Types are first-class entities you can compute with at compile time. This
chapter ends by showing how that single fact subsumes generics, and the
type-system feature set is internally consistent with the rest of the
language: no hidden control flow, no hidden allocation, no ambient
authority, and `comptime` as the only metaprogramming mechanism.

---

## Table of contents

1. [Primitive types](#1-primitive-types)
2. [The `type` type and `void`/`noreturn`](#2-the-type-type-and-voidnoreturn)
3. [Pointers](#3-pointers)
4. [Slices](#4-slices)
5. [Arrays](#5-arrays)
6. [Optionals `?T`](#6-optionals-t)
7. [Error sets and error unions `E!T`](#7-error-sets-and-error-unions-et)
8. [`struct` types](#8-struct-types)
9. [`enum` types](#9-enum-types)
10. [Tagged unions `union(enum)`](#10-tagged-unions-unionenum)
11. [Foreign handles and `*anyopaque`](#11-foreign-handles-and-anyopaque)
12. [Type coercion](#12-type-coercion)
13. [Integer overflow and the arithmetic model](#13-integer-overflow-and-the-arithmetic-model)
14. [Generics are comptime functions returning `type`](#14-generics-are-comptime-functions-returning-type)
15. [Introspection builtins: `@TypeOf`, `@sizeOf`, `@alignOf`, `@typeInfo`](#15-introspection-builtins-typeof-sizeof-alignof-typeinfo)

---

## 1. Primitive types

### 1.1 Sized integers

k2 provides signed and unsigned integers at fixed, machine-independent
bit widths, plus two pointer-sized integers:

| Signed | Unsigned | Width | Notes |
| ------ | -------- | ----- | ----- |
| `i8`   | `u8`     | 8     | `u8` is also the element type of byte strings |
| `i16`  | `u16`    | 16    | |
| `i32`  | `u32`    | 32    | |
| `i64`  | `u64`    | 64    | |
| `i128` | `u128`   | 128   | |
| `isize`| `usize`  | target pointer width | `usize` indexes slices and arrays |

The bit width in the name **is** the representation: an `i32` is exactly
32 bits in two's-complement, a `u16` is exactly 16 unsigned bits, on every
target. `isize`/`usize` are the only integers whose width depends on the
target triple (32 bits on a 32-bit target, 64 on a 64-bit target). Slice
and array lengths, and the indices used to access them, are `usize`.

There is no untyped integer literal at runtime: a literal such as `42`
has the comptime-only type `comptime_int` (see §1.4) until it is coerced
to a concrete sized integer.

```k2
const a: i32 = -7;
const b: u8 = 255;
const c: u64 = 1_000_000;   // underscores are digit separators
const n: usize = some_slice.len;
```

### 1.2 Floats

| Type  | Width | Format |
| ----- | ----- | ------ |
| `f32` | 32    | IEEE-754 binary32 |
| `f64` | 64    | IEEE-754 binary64 |

Float semantics follow IEEE-754: signed zero, infinities, and NaN exist
and propagate per the standard. Float arithmetic does **not** trap on
overflow — it produces `inf` — because that is the defined IEEE behavior,
not a programmer mistake. A float literal such as `3.14` has type
`comptime_float` until coerced (see §1.4).

```k2
const pi: f64 = 3.141592653589793;
const half: f32 = 0.5;
```

### 1.3 `bool`

`bool` has exactly two values, `true` and `false` (both keywords). The
boolean operators are the **word-based** set `and`, `or`, and `not` — this
is a deliberate k2 choice so that the symbolic `!` is reserved strictly
for error unions:

```k2
const ready: bool = true;
const blocked: bool = false;
const go = ready and not blocked;   // `and`, `not` are keywords
```

`and` and `or` short-circuit: the right operand is evaluated only when
needed. This is one of the few non-linear control-flow constructs in the
language, and like all of them it is explicit and named.

### 1.4 `comptime_int` and `comptime_float`

Integer and float literals are not born with a sized type. They have the
arbitrary-precision compile-time types `comptime_int` and `comptime_float`.
A `comptime_int` is a true mathematical integer with no width and no
overflow; a `comptime_float` is an arbitrary-precision float. They exist
only at compile time and can never appear in a runtime variable's type —
they must be coerced (implicitly, when the target type is known, or
explicitly via `@as`) to a sized type to cross into runtime.

```k2
const big = 1 << 100;          // comptime_int: no overflow, no chosen width
const k: u8 = 200;             // 200 coerces to u8 because it fits
const wide: u128 = big >> 30;  // still comptime until assigned a real type

// Compile error: 300 does not fit in u8, caught at compile time.
// const overflow: u8 = 300;
```

Because `comptime_int` has no width, expressions that are fully
compile-time-known never silently overflow; if the *final* coercion to a
sized type would not fit, that is a compile error at the coercion site,
not a wraparound.

---

## 2. The `type` type and `void`/`noreturn`

### 2.1 `type`

`type` is the type of types. A value of type `type` is itself a type —
`i32`, `bool`, `Point`, `[]const u8`, and `?*u32` are all *values* whose
type is `type`. Values of type `type` exist only at compile time; you can
bind them to `const`, pass them as `comptime` parameters, and return them
from functions. This is the whole foundation of k2's generics (§14).

```k2
const Int = i32;                 // Int is a comptime value of type `type`
const Maybe = ?Int;              // building a new type from another
fn pick(comptime flag: bool) type {
    return if (flag) u64 else i64;
}
```

### 2.2 `void`

`void` is the **unit type**: it has exactly one value and carries no
information, so it occupies zero bytes (`@sizeOf(void) == 0`). It is the
return type of functions that produce no value, and the payload type of
the most common entry-point signature, `!void`.

```k2
fn log(out: anytype, msg: []const u8) void {
    // returns nothing meaningful
    _ = out;
    _ = msg;
}
```

The single value of `void` is written `{}` in expression position when a
value is required.

### 2.3 `noreturn`

`noreturn` is the type of an expression that never returns control to its
caller — for example a call to `@panic`, an infinite `while (true)` loop
with no `break`, or `unreachable`. Because `noreturn` is a subtype of
every type, it can appear in any branch of an `if`/`switch` without
constraining the other branches' type.

```k2
fn mustHave(opt: ?u32) u32 {
    return opt orelse unreachable;   // `unreachable` has type noreturn
}
```

### 2.4 `anyerror`

`anyerror` is the open, all-encompassing error set: the supertype of every
error set in the program. It is the escape hatch for code that must erase
the specific set of errors it can produce. It is covered with error
unions in §7.

---

## 3. Pointers

A pointer in k2 is a **single-item pointer**: `*T` points at exactly one
value of type `T`. The const-qualified form `*const T` grants read-only
access to the pointee. There is no implicit null pointer — a pointer is
always valid (absence is modeled with `?*T`, an optional pointer).

| Form        | Meaning |
| ----------- | ------- |
| `*T`        | mutable single-item pointer; `p.*` reads/writes the pointee |
| `*const T`  | read-only single-item pointer |
| `?*T`       | optional pointer; either `null` or a valid `*T` |
| `?*const T` | optional read-only pointer |

The `.*` suffix dereferences. `&expr` takes the address of an addressable
location. The distinction between `*T` (exclusive, mutating) and
`*const T` (shared, read-only) is also k2's primary data-race discipline:
passing `*const T` to several readers is shared access, while `*T` denotes
exclusive access.

```k2
fn bump(p: *u32) void {
    p.* += 1;             // write through the pointer
}

fn read(p: *const u32) u32 {
    return p.*;           // read only; cannot assign through *const
}

pub fn main(sys: *System) !void {
    var x: u32 = 41;
    bump(&x);
    const out = sys.io.stdout();
    try out.print("x = {d}\n", .{read(&x)});
}
```

Note that `main` itself takes `sys: *System` — the root capability handle
is passed by single-item pointer, consistent with this section.

Pointers may be reinterpreted only through the explicit `@ptrCast` builtin
(see §12), which keeps every unsafe pointer cast auditable at its call
site.

---

## 4. Slices

A slice `[]T` is a **pointer plus a length**: a fat pointer to a
contiguous run of `T` values. `[]const T` is a slice of immutable
elements. Slices are the idiomatic way to pass "some number of `T`" across
a function boundary; the length travels with the data, so bounds are
always known.

| Form        | Meaning |
| ----------- | ------- |
| `[]T`       | slice of mutable `T`; `.ptr` and `.len` accessible |
| `[]const T` | slice of immutable `T` |
| `[]const u8`| the canonical k2 string/byte-buffer type |

A slice value exposes `.len` (a `usize`) and supports indexing `s[i]` and
sub-slicing `s[lo..hi]`. In safe builds, every index and sub-slice is
**bounds-checked**; an out-of-bounds access is a programmer mistake and
triggers `@panic` (see §13), not an error value.

```k2
fn sum(xs: []const u32) u32 {
    var total: u32 = 0;
    for (xs) |v| {
        total += v;
    }
    return total;
}

pub fn main(sys: *System) !void {
    const alloc = sys.heap;
    const buf: []u32 = try alloc.alloc(u32, 16);
    defer alloc.free(buf);

    for (buf, 0..) |*slot, i| {
        slot.* = @intCast(i * i);
    }

    const out = sys.io.stdout();
    // `buf` (a []u32) coerces implicitly to the []const u32 that `sum` wants.
    try out.print("sum = {d}\n", .{sum(buf)});
}
```

A `[]T` coerces implicitly to `[]const T` (adding `const` is always safe);
the reverse never happens implicitly. Slices are created by allocating
through an `Allocator` capability (`alloc.alloc(T, n)`), by slicing an
array, or by taking a sub-slice of another slice — k2 never allocates a
slice's backing store on your behalf.

---

## 5. Arrays

An array `[N]T` is a fixed-length, contiguous, **by-value** aggregate of
exactly `N` elements of type `T`, where `N` is a comptime-known `usize`.
Its size is `N * @sizeOf(T)` and it lives wherever it is declared (stack,
static storage, or inside another aggregate) — arrays do not imply any
heap allocation.

| Form     | Meaning |
| -------- | ------- |
| `[N]T`   | array of exactly `N` elements |
| `[_]T`   | array whose length is inferred from its initializer |
| `&arr`   | address-of yields `*[N]T`; `arr[lo..hi]` yields a slice `[]T` |

Arrays are value types: assigning or passing an array copies all `N`
elements. To refer to an array without copying, take a pointer (`*[N]T`)
or a slice (`arr[0..]`).

```k2
const primes = [_]u32{ 2, 3, 5, 7, 11 };   // length inferred as 5
const zeros = [4]i32{ 0, 0, 0, 0 };

fn first(arr: *const [5]u32) u32 {
    return arr[0];
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const view: []const u32 = primes[0..];  // array -> slice
    try out.print("len={d} first={d}\n", .{ view.len, first(&primes) });
}
```

The relationship between the three contiguous forms is worth memorizing:

- `[N]T` — owns its `N` elements by value, length in the type.
- `*[N]T` — a pointer to a whole array, length still in the type.
- `[]T` — a pointer plus a runtime length; the type forgets `N`.

---

## 6. Optionals `?T`

An optional `?T` represents *a value of type `T` or its absence*. It is the
**only** way to model "maybe missing" in k2 — there is no implicit null for
any type, including pointers. The two states are `null` and a wrapped `T`.

You consume an optional by:

- **`orelse`** — supply a default for the `null` case:
  `x orelse default`.
- **payload capture** in `if`/`while` — bind the inner value when present:
  `if (opt) |v| { ... } else { ... }`.

```k2
fn lastByte(bytes: []const u8) ?u8 {
    if (bytes.len == 0) return null;
    return bytes[bytes.len - 1];
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const data: []const u8 = "k2";

    // orelse: provide a fallback value for the null case.
    const tail = lastByte(data) orelse 0;
    try out.print("tail = {d}\n", .{tail});

    // capture: only runs the body when a value is present.
    if (lastByte(data)) |b| {
        try out.print("last char code = {d}\n", .{b});
    } else {
        try out.print("(empty)\n", .{});
    }
}
```

`orelse` is one of k2's explicit non-linear control-flow operators, on the
same footing as `try` and `catch`. Optional pointers (`?*T`) are
guaranteed to use the null pointer representation, so `?*T` has the same
size as `*T` with no extra tag word.

---

## 7. Error sets and error unions `E!T`

### 7.1 Error sets

An **error set** is a type written with the `error` keyword:

```k2
const ParseError = error{ Empty, NotANumber };
```

Each member is an error value — a small integer tag, nothing more. Error
sets are types, so they are comptime values: they can be **merged** with
`||` and compared for subset relationships, and they are introspectable
via `@typeInfo` (see §15). `anyerror` (§2.4) is the open superset of every
error set.

### 7.2 Error unions

An **error union** `E!T` is "either an error from set `E`, or a value of
type `T`". The shorthand `!T` infers the error set `E` from the function
body — the compiler computes the exact set of errors the body can produce.

| Form    | Meaning |
| ------- | ------- |
| `E!T`   | explicit error set `E`, success payload `T` |
| `!T`    | inferred error set, success payload `T` |
| `!void` | the entry-point shape: fails or succeeds with no value |

You consume an error union with:

- **`try expr`** — evaluate `expr`; on error, return that error unchanged
  from the enclosing function (a visible early return — the only sugar in
  the error model).
- **`catch`** — handle the error: `expr catch fallback` substitutes a
  value, or `expr catch |err| { ... }` captures the error into a block.
- **`errdefer stmt`** — run `stmt` only if the scope exits via an error,
  the standard way to release a half-built resource.

```k2
const std = @import("std");

const ParseError = error{ Empty, NotANumber };

/// Combine an allocation error set with the parse error set using `||`.
fn parseDoubled(alloc: Allocator, text: []const u8) (ParseError || error{OutOfMemory})!*u32 {
    if (text.len == 0) return ParseError.Empty;

    const cell: *u32 = try alloc.create(u32);
    errdefer alloc.destroy(cell);   // runs only on a later error in this scope

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

    const ptr = parseDoubled(sys.heap, "21") catch |err| {
        try out.print("parse failed: {s}\n", .{@errorName(err)});
        return;
    };
    defer sys.heap.destroy(ptr);

    try out.print("doubled = {d}\n", .{ptr.*});
}
```

Because errors are ordinary values and the set is a comptime-known type,
callers can exhaustively `switch` over the exact errors a callee may
return — there is no separate exception channel, no stack unwinding, and
no hidden allocation for an error. An error costs the same as any other
branch.

### 7.3 Errors are not the same as programmer mistakes

A returned error (`error{NotFound}`) models an *expected, recoverable*
failure. A **programmer mistake** — indexing out of bounds, integer
overflow in a safe build, reaching `unreachable` — is not an error value:
it triggers `@panic` in safe builds and is undefined behavior in
`ReleaseFast`. Keep the two categories distinct (see §13).

---

## 8. `struct` types

A `struct` is a product type: a fixed set of named fields laid out
contiguously. As the syntax summary states, aggregates are assigned to
`const`s; the type's name is just the binding.

```k2
const Point = struct {
    x: f64,
    y: f64,
};
```

Key facts:

- **Members may be exported** with `pub` (fields, methods, and nested
  declarations). A `pub fn` inside a struct is a method when its first
  parameter is `self`/`*self`-shaped.
- **Methods** are plain `fn` declarations inside the struct body; calling
  `p.dist(q)` is sugar for `Point.dist(p, q)` — no hidden dispatch, no
  vtables unless you build them yourself.
- **`@This()`** returns the enclosing struct type, used to name `Self`
  inside generic structs.
- **Default field values** may be supplied: `count: u32 = 0`.
- Field access is `value.field`; comptime-named access is `@field(value, name)`.

```k2
const std = @import("std");

const Point = struct {
    const Self = @This();

    x: f64,
    y: f64,

    pub fn init(x: f64, y: f64) Self {
        return Self{ .x = x, .y = y };
    }

    pub fn add(self: Self, other: Self) Self {
        return Self{ .x = self.x + other.x, .y = self.y + other.y };
    }
};

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = Point.init(1.0, 2.0);
    const b = Point.init(3.0, 4.0);
    const c = a.add(b);                 // method-call sugar for Point.add(a, b)
    try out.print("c = ({d}, {d})\n", .{ c.x, c.y });
}
```

Struct field order is the declaration order, and the layout is the
compiler's choice unless you opt into a guaranteed layout with `extern`
(C-ABI ordered) or a packed representation for FFI. `@sizeOf` and
`@alignOf` report the chosen layout; `@offsetOf` (where layout is
guaranteed) reports field offsets.

---

## 9. `enum` types

An `enum` is a set of named, distinct integer-backed constants. The tag
type is inferred (smallest unsigned integer that fits) unless you give it
explicitly.

```k2
const Color = enum {
    red,
    green,
    blue,
};

const Status = enum(u8) {     // explicit backing integer type
    ok = 0,
    retry = 1,
    fatal = 255,
};
```

Enums are exhaustively `switch`-able, and the compiler checks that every
variant is handled (or that an `else =>` arm exists). Enums may also carry
methods, just like structs.

```k2
const std = @import("std");

const Color = enum {
    red,
    green,
    blue,

    pub fn name(self: Color) []const u8 {
        return switch (self) {
            .red => "red",
            .green => "green",
            .blue => "blue",
        };
    }
};

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const c: Color = .green;            // `.green` infers the enum from context
    try out.print("color = {s}\n", .{c.name()});
}
```

The leading-dot form (`.green`) is **enum literal** syntax: the variant is
resolved against the expected type, so you do not repeat the enum name when
the context already fixes it. `@typeInfo` exposes an enum's variants and
tag type for reflection (§15).

---

## 10. Tagged unions `union(enum)`

A `union(enum)` is a **sum type**: a value that is exactly one of several
named variants at a time, with a discriminating tag. The tag is itself an
enum, which makes the union exhaustively `switch`-able with payload
capture.

```k2
const std = @import("std");

const Shape = union(enum) {
    circle: f64,                 // payload: radius
    rectangle: struct { w: f64, h: f64 },
    point,                       // a tagless variant carries no payload

    pub fn area(self: Shape) f64 {
        return switch (self) {
            .circle => |r| 3.141592653589793 * r * r,
            .rectangle => |rect| rect.w * rect.h,
            .point => 0.0,
        };
    }
};

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const s: Shape = .{ .circle = 2.0 };
    try out.print("area = {d}\n", .{s.area()});
}
```

Notes:

- The `|r|` / `|rect|` captures bind the active variant's payload inside
  each `switch` arm — this is the same payload-capture mechanism used by
  optionals and error unions.
- The active variant can be queried as an enum value, and the tag enum
  type is available for reflection.
- A bare `union { ... }` (without `(enum)`) is an **untagged** union for
  FFI and manual reinterpretation; it has no discriminant and cannot be
  `switch`-ed safely. Prefer `union(enum)` for ordinary k2 code — it is
  the one obvious way to spell a sum type.

### Relation to other languages

For readers coming from elsewhere, k2's `union(enum)` is the same construct as
**Zig**'s `union(enum)`, intentionally: an explicit tag enum, payload capture in
each `switch` prong, and the option of an untagged bare `union` for FFI. The
representation is also the same idea as a **Rust** `enum` with data (`enum E { A(T),
B }`) or an OCaml/Haskell variant — a *tagged* sum type — but the spelling differs
deliberately:

- The tag is a first-class `enum` you can name and reflect on, not an anonymous
  discriminant. A variant's payload is one type (use a `struct` for several
  fields), so the layout is exactly "tag + a payload area sized to the largest
  variant" with no hidden niche optimization to reason about.
- Matching is the ordinary `switch` with the same `|x|` capture used by optionals
  (`?T`) and error unions (`E!T`); a tagged union is just those two-variant
  shapes generalized to N variants. There is no separate `match` construct.
- Unlike a Rust `enum`, k2 keeps the FFI/manual-reinterpretation case as a
  distinct, clearly-named **untagged** `union { ... }`, rather than folding it
  into the same keyword.

This is downstream of the project's "one obvious way" rule: a sum type is a
`union(enum)`, matched with a `switch`, with no second mechanism to learn.

---

## 11. Foreign handles and `*anyopaque`

Some values have a **size and layout that k2 does not know** — most often a
pointer to a foreign object whose internals a C library owns. k2 models
these with the predeclared type `anyopaque`: an unsized "type of unknown
representation" that exists only behind a pointer. You can form pointers to
it (`*anyopaque`, `?*anyopaque`, `*const anyopaque`) and pass them around,
but you can never create, copy, or dereference an `anyopaque` value
directly, because its representation is hidden. This is the type-system
expression of "we do not know how big this is, and that is intentional."

`anyopaque` is an ordinary predeclared identifier (like `void`, `type`,
and `anyerror` in §5.3 of [the lexical structure](01-lexical-structure.md));
it is **not** a keyword. `*anyopaque` is just `pointer_type` applied to it,
so it is fully covered by the existing grammar and lexer with no new tokens.

The idiomatic pattern for C interop is to take the foreign object as a
`*anyopaque` (a "void pointer" in C terms) and to declare the foreign
functions with `extern`. Because the handle is opaque, you can hold it,
store it, and pass it back to C, but never accidentally copy or stack-store
the object by value — only the C library that defined it can.

```k2
/// `*anyopaque` models "a pointer to some foreign object whose internals
/// C owns." k2 never learns its size, so it cannot be dereferenced or
/// copied by value — only handed back to the C side.
extern fn create_window(width: i32, height: i32) ?*anyopaque;
extern fn destroy_window(w: *anyopaque) void;
extern fn window_show(w: *anyopaque) void;

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    const w = create_window(800, 600) orelse {
        try out.print("could not create window\n", .{});
        return;
    };
    defer destroy_window(w);     // explicit cleanup; no implicit destructor

    window_show(w);
}
```

To give a foreign handle a **distinct, type-checked name** instead of a
bare `*anyopaque`, wrap it in a one-field `struct` (a "newtype") and pass
that struct by pointer. The struct carries only the opaque pointer, so it
costs nothing beyond the pointer it holds, while the type checker keeps a
`*Window` from being confused with some other `*anyopaque`:

```k2
/// A typed wrapper over a foreign window handle. The payload is opaque;
/// only the C library may produce or consume the underlying object.
const Window = struct {
    handle: *anyopaque,
};

extern fn create_window(width: i32, height: i32) ?*anyopaque;
extern fn destroy_window(w: *anyopaque) void;

pub fn open(width: i32, height: i32) ?Window {
    const h = create_window(width, height) orelse return null;
    return Window{ .handle = h };
}

pub fn close(w: Window) void {
    destroy_window(w.handle);
}
```

> **Provisional: named `opaque { ... }` blocks.** A dedicated
> `const Window = opaque {};` form — a named type with no fields and no
> known size, parallel to `struct`/`enum`/`union` — is under consideration
> for a future revision. It is **not** part of the language today: `opaque`
> is **not** one of the locked 35 keywords of
> [§5](01-lexical-structure.md), there is no `opaque_type` production in
> [`docs/grammar.ebnf`](../grammar.ebnf), and the lexer tokenizes `opaque`
> as an ordinary identifier. Until that syntax is reconciled across the
> charter, the keyword table, the grammar, and the lexer, model foreign
> handles with `*anyopaque` (and a wrapper `struct` when you want a
> distinct name) as shown above — the spelling the current grammar and
> implementation already support.

---

## 12. Type coercion

k2 coercions are **explicit by default and lossless when implicit**. The
guiding rule from the philosophy applies directly: an implicit coercion
never calls user code and never loses information. Conversions that could
lose data, reinterpret bits, or narrow a value are spelled with an `@`
builtin so they are visible at the call site.

### 12.1 Implicit coercions (safe, information-preserving)

The compiler will insert these automatically when the target type is
known:

- `comptime_int` → any sized integer that can represent the value
  (checked to fit at compile time).
- `comptime_float` → `f32`/`f64`.
- `T` → `?T` (wrapping a value into an optional).
- `T` → `E!T` and any error → `E!T` (wrapping into an error union).
- `[]T` → `[]const T`, `*T` → `*const T` (adding `const`).
- `*[N]T` → `[]T` (an array pointer to a slice).
- `noreturn` → any type (it never actually produces a value).
- error-set widening: `E1` → `E2` when `E1` is a subset of `E2`, and any
  set → `anyerror`.

```k2
const x: u32 = 7;            // comptime_int 7 -> u32
const maybe: ?u32 = x;       // u32 -> ?u32
var s: []const u8 = "hi";    // string literal -> []const u8
const arr = [_]u8{ 'h', 'i' };
s = arr[0..];                // *[2]u8 / array -> []const u8
```

### 12.2 Explicit coercions (the `@`-builtins)

| Builtin      | Use |
| ------------ | --- |
| `@as(T, e)`  | Lossless, widening coercion to `T`, made visible. The only way to *request* a representation/widening conversion. |
| `@intCast(e)`| Possibly-narrowing integer conversion; **checked** in safe builds (panics on truncation/loss), inferred target from context. |
| `@ptrCast(e)`| Reinterpret a pointer's pointee type; the only way to bit-reinterpret pointers. |

```k2
// @as makes a widening/representation coercion explicit and lossless.
const c: u8 = 'A';
const wide = @as(u32, c);          // 65 as a u32

// @intCast narrows; in safe builds it panics if the value does not fit.
fn squareIndex(i: usize) u32 {
    return @intCast(i * i);        // target u32 inferred from return type
}

// @ptrCast reinterprets a pointer; always auditable at the call site.
fn asBytes(p: *const u32) *const [4]u8 {
    return @ptrCast(p);
}
```

There is no implicit narrowing, no implicit signed/unsigned reinterpretation,
no implicit float↔int conversion, and no implicit pointer reinterpretation.
If you see a value change representation, you can see the `@` builtin that
did it.

---

## 13. Integer overflow and the arithmetic model

Integer overflow is a **programmer mistake**, not an error value, so it is
handled by the safety model rather than the error model:

- In **Debug** and **ReleaseSafe** builds, the standard arithmetic
  operators (`+`, `-`, `*`, and the shift/negate operators) are
  **overflow-checked**. An operation whose mathematical result does not
  fit the operand type triggers `@panic` at the point of overflow.
- In **ReleaseFast**, those checks are stripped and overflow is undefined
  behavior — the contract is "you promised it would not overflow."

k2 deliberately has **no symbolic wrapping or saturating operators**. The
complete set of symbolic tokens is fixed by
[§7.1 of the lexical structure](01-lexical-structure.md), and it does not
include `+%`, `-%`, `*%`, `+|`, `-|`, or `*|`; the lexer would split such
input into two separate tokens (`+` then `%`, `+` then `|`, …), so those
operators do not exist in the language. When the plain `+`/`-`/`*`
operators are not what you want, you choose the overflow behavior with an
**explicit `std.math` function call** instead — keeping the choice visible
at the call site, with no new syntax and no hidden control flow.

| Need | Spelling | Result |
| ---- | -------- | ------ |
| Treat overflow as a *value* | `std.math.add(T, a, b)` (`sub`, `mul`) | `error{Overflow}!T` — handle with `try`/`catch` |
| **Wrap** two's-complement | `std.math.wrapAdd(T, a, b)` (`wrapSub`, `wrapMul`) | `T`, defined on every build mode |
| **Saturate** to the type's bounds | `std.math.satAdd(T, a, b)` (`satSub`, `satMul`) | `T`, clamped, never overflows |

The checked helpers turn overflow into an ordinary `error{Overflow}` value
(useful when an overflow is an *expected input condition*); the `wrap*` and
`sat*` helpers are total functions that are always defined, for the cases —
hashing, checksums, ring buffers — where wraparound or clamping is the
*intended* semantics rather than a bug.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    var h: u8 = 250;
    h = std.math.wrapAdd(u8, h, 10);   // wraps: 250 + 10 == 4 (mod 256)
    try out.print("wrapped = {d}\n", .{h});

    var s: u8 = 250;
    s = std.math.satAdd(u8, s, 10);    // saturates to 255, never overflows
    try out.print("saturated = {d}\n", .{s});

    // Overflow as a value: handled with `catch`, not a panic.
    const total: u8 = std.math.add(u8, 200, 100) catch {
        try out.print("would overflow u8\n", .{});
        return;
    };
    try out.print("total = {d}\n", .{total});

    // The plain operator below would @panic in safe builds if it overflowed:
    var n: u32 = 3;
    n = n * 4;            // 12 — fine; checked in Debug/ReleaseSafe
    try out.print("n = {d}\n", .{n});
}
```

This split keeps the common case safe by default (catch the bug) while
making the rare, deliberately-wrapping or -saturating case an explicit,
zero-cost function call. The same checked-vs-stripped distinction governs
slice/array bounds checks and the `@intCast` narrowing check described in
§12.

Division and remainder by zero, and signed overflow from negating the
minimum value, are likewise checked safety violations in safe builds. For
fully comptime-known arithmetic on `comptime_int`/`comptime_float` there is
no overflow at all (§1.4) — arbitrary precision means the only failure is a
too-large *final coercion*, caught at compile time.

---

## 14. Generics are comptime functions returning `type`

k2 has **no separate generics grammar**, no template syntax, and no macro
language. Because `type` is a first-class comptime value (§2.1), a generic
is simply a function that takes one or more `comptime` parameters and
returns a `type`. The compiler instantiates and caches the result per
distinct set of comptime arguments (monomorphization).

This is the single mechanism, learned once:

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

pub fn main(sys: *System) !void {
    var nums = List(u32).init(sys.heap);   // List(u32) is an ordinary call
    defer nums.deinit();
    try nums.push(40);
    try nums.push(2);
    const out = sys.io.stdout();
    try out.print("sum = {d}\n", .{nums.items[0] + nums.items[1]});
}
```

`List(u32)` is an ordinary function call evaluated at compile time; its
result is a struct type, which you then use exactly like any hand-written
struct. `List(u32)` and `List(f64)` are distinct, independently
monomorphized types.

Constraints are expressed with ordinary comptime code, not a trait
grammar. Use `@typeInfo`/`@hasField` to inspect the type argument and
`@compileError` to reject invalid instantiations with a precise,
source-located diagnostic:

```k2
/// Require a numeric element type; reject anything else at compile time.
fn Vector(comptime T: type) type {
    const info = @typeInfo(T);
    if (info != .Int and info != .Float) {
        @compileError("Vector requires an integer or float element, got " ++ @typeName(T));
    }
    return struct {
        const Self = @This();
        data: [3]T,

        pub fn zero() Self {
            return Self{ .data = [_]T{ 0, 0, 0 } };
        }
    };
}
```

Generic *functions* work the same way — a `comptime` type parameter (or an
`anytype` parameter whose type is inferred at the call site) makes the
function polymorphic without any special syntax:

```k2
fn maxOf(comptime T: type, a: T, b: T) T {
    return if (a > b) a else b;
}

// `anytype` infers the parameter type from the argument via @TypeOf.
fn identity(x: anytype) @TypeOf(x) {
    return x;
}
```

Type-returning functions, `comptime` value parameters, and `anytype`
together cover the entire generics design space with one rule: *it is just
comptime k2 code that computes types and values.*

---

## 15. Introspection builtins: `@TypeOf`, `@sizeOf`, `@alignOf`, `@typeInfo`

The type system is **reified**: types can be inspected and constructed by
ordinary comptime code. The core builtins are summarized here; they all run
at compile time.

### 15.1 `@TypeOf`

`@TypeOf(expr, ...)` returns the *type* of one or more expressions without
evaluating them at runtime. It is the basis for type inference in generic
code — most commonly to name the return type of an `anytype` function.

```k2
const x: u64 = 9;
const T = @TypeOf(x);          // T == u64, a comptime value of type `type`

fn doubled(v: anytype) @TypeOf(v) {
    return v + v;              // return type follows the argument's type
}
```

### 15.2 `@sizeOf` and `@alignOf`

`@sizeOf(T)` is the comptime byte size of `T`; `@alignOf(T)` is its
alignment requirement. Both are used for manual layout, allocation sizing,
and FFI. Recall the boundary cases: `@sizeOf(void) == 0`, and
`@sizeOf(anyopaque)` is not available because the layout is unknown (§11) —
an `anyopaque` may only ever be handled behind a pointer.

```k2
const std = @import("std");

const Point = struct { x: f64, y: f64 };

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("size={d} align={d}\n", .{ @sizeOf(Point), @alignOf(Point) });
    try out.print("u32 is {d} bytes\n", .{@sizeOf(u32)});
}
```

### 15.3 `@typeInfo` and `@Type`

`@typeInfo(T)` reflects on a type, yielding a tagged-union description you
can pattern-match: the variant tells you whether `T` is a `.Struct`,
`.Enum`, `.Union`, `.Int`, `.Float`, `.Pointer`, `.Optional`,
`.ErrorSet`, `.ErrorUnion`, and so on, with the fields, variants, or
signature attached. `@Type` is the inverse — it constructs a new type from
such a description, enabling serializers, ABI shims, and trait-style checks
written as plain k2.

For a `union(enum)`, `@typeInfo(U).Union` carries `tag_type` (the
discriminant's integer type) and `fields` — one descriptor per variant with
its `name` and payload `type` (a payload-less variant's `type` is `void`),
the same `name : type` shape a struct field has. So a generic that walks a
union's variants reads `inline for (@typeInfo(U).Union.fields) |v| { … }`
exactly as it would walk a struct's fields.

```k2
const std = @import("std");

/// Generate a field-by-field printer for ANY struct type at comptime.
/// No macros: this is ordinary k2 run by the compiler.
fn printFields(comptime T: type, out: anytype, value: T) !void {
    const info = @typeInfo(T);
    if (info != .Struct) {
        @compileError("printFields requires a struct, got " ++ @typeName(T));
    }
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

The same reflection lets libraries enumerate an enum's variants, walk a
tagged union's payload types, or exhaustively `switch` over the members of
an error set — all at compile time, with no runtime cost and no macro
language. `@typeInfo`/`@Type` is the reified-reflection round-trip that
makes k2's metaprogramming a single, learnable mechanism rather than a
separate dialect.

---

## Summary

- **Primitives**: exact-width integers `i8…i128`/`u8…u128` plus
  `isize`/`usize`; floats `f32`/`f64`; `bool` with word operators
  `and`/`or`/`not`; the unit type `void`; `noreturn`; the first-class
  `type`; and the comptime-only `comptime_int`/`comptime_float`.
- **Composite forms** are postfix modifiers: `*T`/`*const T` pointers,
  `[]T`/`[]const T` slices, `[N]T` arrays, `?T` optionals, and `E!T`/`!T`
  error unions — with `null`/`orelse`/capture for optionals and
  `try`/`catch`/`errdefer` for error unions.
- **Aggregates** are `struct` (product), `enum` (named integer set), and
  `union(enum)` (tagged sum); foreign objects of unknown layout are modeled
  with `*anyopaque` (a wrapper `struct` when a distinct name is wanted).
- **Coercion** is implicit only when lossless; every narrowing, widening
  request, or pointer reinterpretation is an explicit `@as`/`@intCast`/
  `@ptrCast`. Integer overflow is checked in safe builds (`@panic`); when
  wraparound, saturation, or overflow-as-a-value is intended, you call the
  explicit `std.math.wrapAdd`/`satAdd`/`add` helpers — k2 has no symbolic
  wrapping/saturating operators.
- **Generics** need no new syntax: a generic is a `comptime` function
  returning `type`, instantiated and cached per argument, with
  `@TypeOf`/`@typeInfo`/`@compileError` providing inference, reflection,
  and constraints.

This keeps the whole type system inside k2's single metaprogramming
mechanism — `comptime` — and consistent with the language's promises: no
hidden control flow, no hidden allocation, no ambient authority, and one
obvious way to spell each concept.
