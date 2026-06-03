# 03 — Expressions and Statements

This chapter specifies k2's expression grammar, the way blocks act as
expressions, and the complete, *closed* set of control-flow constructs. k2 is
expression-oriented: most constructs that look like statements in a C-family
language — `if`, `switch`, blocks — produce values.

The guiding pillar throughout is **No hidden control flow**. Reading k2 code,
you can see exactly what runs. The only non-linear control flow is the explicit
set:

```
if / else   while   for   switch   try   catch
defer   errdefer   orelse   return   break   continue   unreachable
```

There are no exceptions, no destructors firing on scope exit, no operator
overloading dispatching to user code, and no implicit coercions that call
conversion functions. A function call looks like a function call; everything
else is plain data movement.

---

## 1. Expressions and statements

An **expression** computes a value. A **statement** is an expression (or
declaration) used for its effect, terminated by `;`. Because k2 is
expression-oriented, the boundary is thin: an `if`, a `switch`, or a `{ ... }`
block can be used either as a value (assigned, passed, returned) or as a
statement (evaluated for effect and discarded).

```k2
const a = 1 + 2 * 3;          // expression bound to a const
const b = if (a > 5) 10 else 0; // `if` used as an expression

doWork();                     // expression-statement: result discarded
```

A bare expression-statement whose result is not `void` must explicitly discard
the value by assigning it to `_`:

```k2
_ = mightReturnSomething();   // value intentionally ignored
```

This is part of **No hidden control flow** and the broader honesty discipline:
silently dropping a meaningful result — especially an error union — is never
implicit.

### 1.1 Operators

k2 uses a **fully word-based** boolean operator set. Logical operators are the
keywords `and`, `or`, and `not`; the symbolic `!` is reserved *strictly* for
error-union types and is never boolean negation.

| Category    | Spelling                                  |
| ----------- | ----------------------------------------- |
| Arithmetic  | `+`  `-`  `*`  `/`  `%`                    |
| Comparison  | `==`  `!=`  `<`  `<=`  `>`  `>=`           |
| Logical     | `and`  `or`  `not`                        |
| Bitwise     | `&`  `\|`  `^`  `~`  `<<`  `>>`            |
| Assignment  | `=`  `+=`  `-=`  `*=`  `/=`  `%=` (etc.)   |

```k2
const ready = a > 0 and not done;   // word-based logicals
const mask  = flags & 0x0F;         // bitwise AND
```

`and` and `or` short-circuit: the right operand is evaluated only when needed.
This is the one place ordering matters, and it is explicit in the spelling.

There is **no operator overloading**. `+` on two `f64`s is hardware addition,
never a call into user code. Likewise, there are no implicit numeric coercions
that invoke conversion functions: widening is written with `@as`, narrowing
with `@intCast`, both visible at the call site.

```k2
const wide: u32 = @as(u32, small_u8);  // explicit, lossless widening
const narrow: u8 = @intCast(big_u32);  // explicit, checked narrowing
```

---

## 2. Blocks as expressions

A block `{ ... }` introduces a new scope. As a *statement*, it groups
declarations and statements. As an *expression*, it must yield a value, which
is produced by `break` out of a **labeled** block.

A label is written `name:` before the opening brace; `break :name value`
supplies the block's result:

```k2
const clamped = blk: {
    if (x < lo) break :blk lo;
    if (x > hi) break :blk hi;
    break :blk x;
};
```

A block used purely for effect needs no label and yields `void`:

```k2
{
    const tmp = a;
    a = b;
    b = tmp;
}
```

Declarations inside a block are scoped to that block. There is no implicit
cleanup when the block exits — see [§9 `defer` / `errdefer`](#9-defer-and-errdefer)
for the explicit mechanism.

---

## 3. `if` / `else`

`if` is both a statement and an expression. The condition must be `bool` (there
are no truthy coercions).

As an expression, both arms must produce compatible types and an `else` is
required:

```k2
const sign = if (n < 0) -1 else if (n > 0) 1 else 0;
```

As a statement, `else` is optional:

```k2
if (out_of_range) {
    return error.OutOfRange;
}
```

### 3.1 Optional capture

`if` can test an optional (`?T`) and bind its payload with a capture `|x|`. The
capture is in scope only in the `then` branch; the `else` branch handles the
`null` case:

```k2
// `find` returns ?usize
if (find(items, target)) |index| {
    try out.print("found at {d}\n", .{index});
} else {
    try out.print("not found\n", .{});
}
```

Capture by reference with `|*x|` when you intend to mutate through the payload:

```k2
if (table.lookup(key)) |*entry| {
    entry.*.count += 1;   // mutate the optional's payload in place
}
```

### 3.2 Error-union capture

`if` can also destructure an error union, binding the success payload in the
`then` branch and the error in the `else` branch with `|err|`:

```k2
if (parseConfig(text)) |config| {
    use(config);
} else |err| {
    try out.print("config error: {s}\n", .{@errorName(err)});
}
```

This is the explicit, branch-based way to inspect an error without propagating
it. For propagation use `try` ([Chapter 04](04-functions.md)); for handling
inline use `catch` ([§8](#8-orelse-and-catch)).

---

## 4. `while`

`while` loops while a `bool` condition holds. It is an expression: its value is
the operand of a `break`, or the value of the optional `else` clause when the
loop finishes without breaking.

```k2
var i: usize = 0;
while (i < n) {
    process(items[i]);
    i += 1;
}
```

### 4.1 Continue expression

A `while` may carry a *continue expression* in `: ( ... )`, run after each
iteration and before the next condition test. This keeps the loop's stepping
visible at the top:

```k2
var i: usize = 0;
while (i < n) : (i += 1) {
    process(items[i]);
}
```

### 4.2 Optional capture (`while (opt) |x|`)

When the condition is an optional-producing expression, `while` loops as long as
the expression is non-`null`, binding the payload each iteration. The loop ends
on the first `null`:

```k2
// `it.next()` returns ?Token
while (it.next()) |token| {
    try out.print("token: {s}\n", .{token.text});
}
```

This combines naturally with a continue expression and `|*x|` reference capture.

### 4.3 Error-union capture and `else`

`while` over an error-union-producing expression captures the payload and routes
errors through an `else |err|` clause, which also receives a normal loop's
non-break completion:

```k2
var sum: u64 = 0;
const total = while (stream.nextChunk()) |chunk| {
    sum += chunk.len;
} else |err| {
    return err;   // propagate a stream error explicitly
};
_ = total;
```

The plain (non-error) `else` form gives a value for "the loop completed without
`break`":

```k2
const found = while (i < n) : (i += 1) {
    if (items[i] == target) break true;
} else false;
```

---

## 5. `for`

`for` iterates over slices, arrays, and integer ranges. Each operand gets a
capture in the `|...|` list. `for` is an expression with the same `break` /
`else` value semantics as `while`.

### 5.1 Over slices and arrays

```k2
for (items) |item| {
    consume(item);
}
```

Capture by reference with `|*item|` to mutate elements in place:

```k2
for (buf) |*slot| {
    slot.* = 0;   // zero the buffer through references
}
```

### 5.2 Index via a range operand

A second operand `0..` supplies a running index. This is the canonical way to
get an index — there is one obvious spelling:

```k2
for (buf, 0..) |*slot, i| {
    slot.* = @intCast(i * i);
}
```

### 5.3 Over a bounded range

A bounded range `lo..hi` (half-open, `hi` excluded) iterates without any backing
collection:

```k2
for (0..n) |i| {
    try out.print("{d}\n", .{i});
}
```

### 5.4 Multiple operands in lockstep

Several operands iterate together; all must have the same length (checked in
safe builds). This zips two slices without an explicit index:

```k2
for (keys, values) |key, value| {
    try map.put(key, value);
}
```

### 5.5 `for` with `else`

Like `while`, a `for` can yield a "completed without break" value:

```k2
const idx = for (items, 0..) |item, i| {
    if (item == target) break i;
} else items.len;   // sentinel meaning "not found"
```

---

## 6. `switch`

`switch` selects an arm by matching a value against patterns. Arms use `=>`.
Switching over an enum, an error set, or a tagged union is **exhaustive**: every
variant must be handled, or an explicit `else` arm must be present. Omitting a
case is a compile error, which is how k2 makes adding a new variant a
compile-time obligation rather than a silent fall-through.

`switch` is an expression; each arm produces the result value.

### 6.1 Enum switch (exhaustive)

```k2
const Dir = enum { north, south, east, west };

const dx: i32 = switch (dir) {
    .north => 0,
    .south => 0,
    .east => 1,
    .west => -1,
};   // no `else` needed: all variants covered
```

### 6.2 Multiple values and ranges

A single arm may match several scalar values (separated by `,`) or an inclusive
range with `...`:

```k2
const kind = switch (byte) {
    '0'...'9' => Kind.digit,
    'a'...'z', 'A'...'Z' => Kind.letter,
    ' ', '\t', '\n' => Kind.space,
    else => Kind.other,
};
```

### 6.3 Captures and tagged unions

Switching over a tagged `union(enum)` binds the active payload with `|value|`
(or `|*value|` to mutate it):

```k2
const Node = union(enum) {
    leaf: i32,
    pair: struct { lhs: i32, rhs: i32 },
};

const total: i32 = switch (node) {
    .leaf => |v| v,
    .pair => |p| p.lhs + p.rhs,
};
```

### 6.4 Block arms

An arm whose body is a block yields its value with a labeled `break`:

```k2
const label = switch (code) {
    0 => "ok",
    else => blk: {
        const tag = lookupTag(code);
        break :blk tag;
    },
};
```

### 6.5 Exhaustiveness over error sets

Because error sets are introspectable types, `switch` can exhaustively match the
exact errors a callee may return:

```k2
fn classify(err: OpenError) Severity {
    return switch (err) {
        error.NotFound => Severity.warn,
        error.PermissionDenied => Severity.fatal,
        error.OutOfMemory => Severity.fatal,
    };
}
```

---

## 7. Labeled loops, `break`, and `continue`

`break` exits the nearest enclosing loop (or labeled block); `continue` skips to
the next iteration. Both may target a **label** to act on an *outer* loop,
written `name:` before the loop keyword.

```k2
outer: for (rows) |row| {
    for (row) |cell| {
        if (cell == sentinel) continue :outer; // skip rest of this row
        if (cell == bomb) break :outer;        // leave both loops
        visit(cell);
    }
}
```

A labeled `break` may also carry a value out of a loop-as-expression:

```k2
const hit = outer: for (grid, 0..) |row, y| {
    for (row, 0..) |cell, x| {
        if (cell == target) break :outer Point{ .x = @intCast(x), .y = @intCast(y) };
    }
} else null;   // `?Point`: `null` when nothing matched
```

`break` and `continue` are the *only* intra-loop jumps; there is no `goto`.

---

## 8. `orelse` and `catch`

`orelse` and `catch` are the two expression-level operators for unwrapping
absence and error, respectively. Both expand to a visible branch — there is no
hidden control flow.

### 8.1 `orelse` — optionals

`opt orelse default` evaluates `opt`; if it is non-`null`, the result is the
unwrapped payload, otherwise the result is `default`. The right side is
evaluated only on `null` (short-circuit).

```k2
const port: u16 = config.port orelse 8080;   // default when absent
```

The right side may be any expression of the payload type, including a block that
breaks a value, or a diverging expression such as `return` or `unreachable`:

```k2
const handle = open(path) orelse return error.NoHandle;
const first = list.head orelse unreachable; // we proved it is non-null
```

### 8.2 `catch` — error unions

`catch` handles the error arm of an error union. With a plain right-hand side it
supplies a fallback value:

```k2
const count = parseCount(text) catch 0;   // default on any error
```

With a capture `|err|` it runs a block that handles the specific error and must
either produce a value, propagate, or diverge:

```k2
const ptr = parseDoubled(sys.heap, "21") catch |err| {
    try out.print("parse failed: {s}\n", .{@errorName(err)});
    return;
};
```

`catch` handles errors inline; `try` (Chapter 04) propagates them unchanged.
They are complementary halves of the same value-based error model.

---

## 9. `defer` and `errdefer`

k2 has **no destructors** and nothing runs implicitly on scope exit. Cleanup is
written explicitly with `defer` and `errdefer`, which schedule a statement to
run when the enclosing block exits.

### 9.1 `defer`

`defer stmt;` runs `stmt` when the enclosing block exits **by any path** —
fall-through, `return`, `break`, `continue`, or error propagation. Deferred
statements run in **reverse (LIFO)** order of registration. The idiomatic
pattern pairs an acquisition with its release in the same scope:

```k2
const buf: []u32 = try alloc.alloc(u32, 16);
defer alloc.free(buf);   // released no matter how this scope ends
```

Because evaluation is LIFO, nested resources unwind in the correct order:

```k2
const file = try sys.io.open(path);
defer file.close();           // runs second

const map = try mapFile(file);
defer unmap(map);             // runs first
```

### 9.2 `errdefer`

`errdefer stmt;` runs `stmt` **only** when the enclosing scope exits via an
**error**. It is the standard way to release a half-built resource while letting
a fully constructed one survive:

```k2
const cell: *u32 = try alloc.create(u32);
errdefer alloc.destroy(cell);   // freed ONLY if a later step errors

try validate(cell);             // if this errors, `cell` is destroyed
return cell;                    // success: ownership transfers to caller
```

On the success path the `errdefer` does **not** run, so ownership passes out of
the function intact. On the error path it fires before the error propagates,
preventing a leak. This pairing — `defer` for unconditional cleanup, `errdefer`
for error-only cleanup — is the whole of k2's resource management. There is no
reference counting and no implicit finalizer behind it.

---

## 10. `unreachable`

`unreachable` asserts that control flow can never reach a point. It has type
`noreturn`, so it composes with `orelse`, `catch`, and `switch` arms that must
yield a value.

In **safe builds** (Debug and ReleaseSafe), reaching `unreachable` triggers an
explicit `@panic`. In **ReleaseFast** it is undefined behavior: the optimizer is
permitted to assume the branch is dead. Reaching `unreachable` is a *programmer
mistake*, not an error value — it never participates in error unions.

```k2
const name = switch (level) {
    0 => "low",
    1 => "mid",
    2 => "high",
    else => unreachable,   // caller guarantees level is in 0..=2
};
```

Use `unreachable` to encode an invariant you have already established; use a
real `error` value for conditions a caller might legitimately hit.

---

## 11. The closed set, restated

Every non-linear control transfer in k2 is one of:
`if`/`else`, `while`, `for`, `switch`, `try`, `catch`, `defer`, `errdefer`,
`orelse`, `return`, `break`, `continue`, and `unreachable`. There is nothing
else — no exceptions, no `goto`, no implicit destructor calls, no hidden
coercions. If you cannot point to one of these keywords, control simply falls
through in source order. That is the entire promise of **No hidden control
flow**.
