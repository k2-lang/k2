# 04 — Functions

Functions are k2's unit of computation and, because **comptime is the only
metaprogramming**, also its unit of generics. There is no separate template or
macro grammar: a generic is an ordinary function that takes `type` values at
compile time and returns a type. This chapter specifies how functions are
declared, how parameters and results behave, what `pub`, `comptime`, `inline`,
and `extern` mean on a function, how errors are returned, and how the same
`fn` machinery expresses generics.

---

## 1. Function declarations

A function is written:

```k2
fn name(param: T, ...) ReturnType { ... }
```

The return type appears to the right of the parameter list, consistent with
k2's left-to-right, postfix-modifier type style. A function with no useful
result returns `void`:

```k2
fn swap(a: *i32, b: *i32) void {
    const tmp = a.*;
    a.* = b.*;
    b.* = tmp;
}
```

A function that computes a value names its type:

```k2
fn add(a: i32, b: i32) i32 {
    return a + b;
}
```

`return expr;` yields the function's result. A `void` function may use a bare
`return;` for an early exit, or simply fall off the end. There is no implicit
return value and no fall-through that fabricates one.

### 1.1 `pub` — export from the module

A top-level `fn` is private to its module unless prefixed with `pub`. `pub`
makes the function part of the module's namespace as returned by `@import`. The
same rule applies to functions declared inside a `struct`.

```k2
pub fn parse(text: []const u8) !Value { ... }   // visible to importers
fn helperInternal(x: i32) i32 { ... }           // module-private
```

### 1.2 The entry point

Every k2 program's entry point has a fixed signature:

```k2
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
}
```

`sys: *System` is the **root capability handle** — the sole source of authority
in the program, consistent with **No ambient authority**. There is no ambient
stdout, clock, allocator, or RNG; narrower capabilities (`sys.heap`, `sys.io`,
`sys.clock`, `sys.random`, `sys.env`, `sys.net`) are obtained from `sys` and
threaded downward as explicit parameters. A function's signature is therefore an
honest, complete account of what it can do.

The `!void` return type means `main` may fail with an inferred error set; a
returned error is reported by the runtime as the process result.

---

## 2. Parameters

Parameters are immutable bindings within the function body — you cannot reassign
a parameter name. To mutate a caller's value, pass a pointer and write through
it; this keeps mutation visible at the call site.

```k2
fn increment(counter: *u64) void {
    counter.* += 1;   // mutates the caller's variable via pointer
}
```

### 2.1 Passing by value vs. pointer

Small values are passed by value. To share or mutate larger aggregates, pass a
pointer. The pointer's constness is the data-sharing contract of k2's
concurrency model:

- `*T` — exclusive, mutable access.
- `*const T` — shared, read-only access.

```k2
fn sum(values: []const u32) u64 {          // borrows, never mutates
    var total: u64 = 0;
    for (values) |v| total += v;
    return total;
}

fn fill(values: []u32, x: u32) void {      // mutates through the slice
    for (values) |*slot| slot.* = x;
}
```

There is no implicit boxing and no copy that calls user code; passing a value is
plain data movement.

### 2.2 Capabilities are ordinary parameters

Because there is no ambient authority, effects arrive as parameters like any
other value. A function that takes only an `Allocator` provably cannot read the
clock or touch the network — the signature is the audit.

```k2
fn loadTable(alloc: Allocator, text: []const u8) !Table { ... }
```

Capabilities are plain structs of function pointers plus a context pointer, so
they cost nothing beyond an indirect call and are frequently specialized away by
the optimizer when the concrete capability is known.

### 2.3 `anytype` parameters

A parameter typed `anytype` is resolved per call site from the argument's type,
inferred via `@TypeOf`. It is the idiom for accepting "any writer", "any
iterator", or similar duck-typed values without a nominal interface:

```k2
fn writeAll(out: anytype, bytes: []const u8) !void {
    try out.print("{s}", .{bytes});
}
```

`anytype` makes the function generic over the argument's type; see [§7](#7-functions-as-generics).

---

## 3. `comptime` parameters

A parameter marked `comptime` must be supplied a value known at compile time.
This is the single mechanism behind generics, since `type` is itself a
first-class comptime value.

```k2
fn replicate(comptime n: usize, value: u8) [n]u8 {
    var arr: [n]u8 = undefined;
    for (&arr) |*slot| slot.* = value;
    return arr;
}

const line = replicate(8, '-');   // n is comptime-known; result is [8]u8
```

Passing a runtime value where a `comptime` parameter is required is a compile
error. The compiler instantiates and caches one specialization per distinct set
of comptime arguments during monomorphization.

`@compileError` is the standard way to reject an invalid comptime argument with
a precise, source-located diagnostic:

```k2
fn bitsOf(comptime T: type) comptime_int {
    return switch (@typeInfo(T)) {
        .Int => |info| info.bits,
        else => @compileError("bitsOf expects an integer type, got " ++ @typeName(T)),
    };
}
```

---

## 4. Returning errors: `!T` and `E!T`

Failures are values, never exceptions. A function that may fail returns an
**error union**:

- `E!T` — explicit error set `E`, the function may return any error in `E` or a
  value of type `T`.
- `!T` — the error set is **inferred** from the function body (the union of
  every error it returns or propagates with `try`).

```k2
const ParseError = error{ Empty, NotANumber };

// Explicit, merged error set; error sets are types and mergeable with `||`.
fn parseDoubled(alloc: Allocator, text: []const u8) (ParseError || error{OutOfMemory})!*u32 {
    if (text.len == 0) return ParseError.Empty;

    const cell: *u32 = try alloc.create(u32);
    errdefer alloc.destroy(cell);

    var value: u32 = 0;
    for (text) |ch| {
        if (ch < '0' or ch > '9') return ParseError.NotANumber;
        value = value * 10 + @as(u32, ch - '0');
    }
    cell.* = value * 2;
    return cell;
}
```

### 4.1 `try` — propagate

`try expr` evaluates `expr`; on error it returns that error from the enclosing
function unchanged, otherwise it yields the success payload. It is the only
sugar in the error model, and it expands to a visible early `return`. The
enclosing function must itself return an error union whose set is a superset of
the callee's.

```k2
fn run(sys: *System) !void {
    const cfg = try loadConfig(sys.heap);   // propagate on failure
    try apply(cfg);
}
```

### 4.2 `catch` — handle inline

`catch` handles the error arm with a fallback value or a capturing block, as
specified in [Chapter 03 §8](03-expressions-and-statements.md#8-orelse-and-catch).

### 4.3 `errdefer` — clean up the error path

`errdefer` (see [Chapter 03 §9](03-expressions-and-statements.md#9-defer-and-errdefer))
releases a half-built resource on the error path only. The `parseDoubled`
example above destroys `cell` if a later step errors and transfers ownership to
the caller on success.

### 4.4 `anyerror` and introspection

`anyerror` is the open superset for code that must erase the specific set. Where
possible prefer a concrete set: error sets are introspectable via `@typeInfo`,
so callers can exhaustively `switch` over exactly the errors a function may
return.

A returned error is just a small integer tag — there is no hidden allocation for
errors and no implicit error-context object. Programmer mistakes
(out-of-bounds, overflow in safe builds, reaching `unreachable`) are **not**
errors and never appear in an error set; they trigger `@panic` in safe builds.

---

## 5. `inline` functions

`inline fn` requests that every call be expanded at the call site rather than
emitted as a callable. Use it for tiny wrappers or where the body must be
specialized to its comptime arguments. As with all of k2, inlining changes code
generation, never semantics.

```k2
inline fn min(a: i64, b: i64) i64 {
    return if (a < b) a else b;
}
```

This is distinct from the `inline for` / `inline while` *loop* forms, which
unroll a loop over a comptime-known sequence (used in reflection code, see
[§7](#7-functions-as-generics)).

---

## 6. Calling conventions, `extern`, and `export`

k2's default calling convention is unspecified and internal: the compiler is
free to pass arguments however the target ABI and optimizer prefer, which is
what lets capability indirections collapse to hand-written-equivalent code.

For C interoperability — a first-class concern, not an afterthought — k2 uses
the C ABI explicitly:

- `extern fn` declares a function implemented elsewhere (typically C), to be
  resolved at link time. It has no body.
- `export fn` gives a k2 function the C ABI and external linkage so C can call
  it.

```k2
// Imported from a C library, resolved by the cross-compilation-aware linker.
extern fn c_strlen(s: [*:0]const u8) usize;

// Exposed to C callers with the C ABI and a stable symbol name.
export fn k2_add(a: i32, b: i32) i32 {
    return a + b;
}
```

`extern`/`export` participate in the integrated C-translation and
cross-compilation path: the target triple is a build parameter, and the driver
links the target's system libraries directly — no separate toolchain juggling.

---

## 7. Functions as generics

Generics are not a separate language feature. A generic is a function whose
`comptime` parameters include `type` values, and which may **return a type**
(its return type is `type`). The compiler runs the function at comptime,
producing and caching a concrete type per distinct instantiation.

### 7.1 A function that returns a type

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

Instantiation is an ordinary call with a comptime type argument:

```k2
var nums = List(u32).init(sys.heap);
defer nums.deinit();
try nums.push(40);
try nums.push(2);
```

`List(u32)` and `List(u8)` are distinct, separately cached types. Methods like
`init`, `deinit`, and `push` are simply `pub fn`s declared inside the returned
struct; the first parameter `self: *Self` (or `self: Self`) is the receiver,
called with dot syntax (`nums.push(2)`).

### 7.2 Type inference with `@TypeOf` and `anytype`

When a generic function should accept any argument and adapt to its type, use
`anytype` and recover the type with `@TypeOf`:

```k2
fn dupePair(comptime _: void, x: anytype) [2]@TypeOf(x) {
    return .{ x, x };
}
```

### 7.3 Reflection-driven generics

Because `@typeInfo` reifies a type into structured data, generic functions can
inspect and generate code over a type's fields. `inline for` unrolls the
comptime-known field list:

```k2
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
```

This is the whole of k2's metaprogramming surface: `comptime` parameters,
functions returning `type`, the `@`-builtins (`@TypeOf`, `@typeInfo`, `@Type`,
`@hasField`, `@field`, `@compileError`), and ordinary control flow — one
mechanism, learned once, used everywhere.

---

## 8. Summary

- Functions read `fn name(params) ReturnType { ... }`; `pub fn` exports,
  `comptime` on a parameter forces a compile-time argument, `inline` expands at
  the call site, `extern`/`export` bridge the C ABI.
- Parameters are immutable bindings; mutate through `*T`, share through
  `*const T`. Effects, including the allocator and all of `*System`, are passed
  explicitly — the signature is the complete account of a function's authority.
- Failures are values: return `!T` (inferred set) or `E!T` (explicit set),
  propagate with `try`, handle with `catch`, clean up with `errdefer`.
- Generics are just functions over `type` values evaluated at comptime; there is
  no template or macro grammar.
