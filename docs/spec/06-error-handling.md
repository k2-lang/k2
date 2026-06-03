# 6. Error Handling

> **k2 language specification — chapter 6**
> Status: normative. This chapter defines how k2 represents, propagates, handles,
> and reflects on failure.

In k2, **errors are values**. A failure is an ordinary value carried in an *error
union*, not a separate control-flow channel layered on top of the language. There
are no stack-unwinding exceptions, no implicit `catch` frames, no error context
objects threaded behind your back, and no hidden allocation for the act of
failing. An error value is just a small integer tag.

This is a direct consequence of two of k2's pillars:

- **No hidden control flow.** Reading a function, you can see every place it can
  exit early. The *only* error-related sugar is `try`, and `try` expands to a
  visible early `return`. Everything else — `catch`, `errdefer`, `orelse` — is an
  ordinary expression or a scoped statement with no surprises.
- **Errors are values.** Because failures are plain data, error handling has the
  *same cost model as any other branch*. There is no separate, slow "exceptional"
  path; there is just a comparison and a jump, exactly as if you had written the
  branch by hand.

This chapter assumes familiarity with k2's declaration forms (`const`/`var`),
function syntax, and the postfix-modifier type grammar (`?T`, `!T`, `E!T`, `*T`,
`[]T`) introduced in earlier chapters.

---

## 6.1 Error sets

An **error set** is a type. It is written with the `error` keyword followed by a
brace-enclosed list of *error names*:

```k2
const ParseError = error{ Empty, NotANumber };
```

Each name in an error set (here `Empty` and `NotANumber`) denotes a distinct
**error value**. An error value of set `E` is referred to with the set name as a
namespace:

```k2
const ParseError = error{ Empty, NotANumber };

fn first(text: []const u8) ParseError!u8 {
    if (text.len == 0) return ParseError.Empty;
    return text[0];
}
```

### 6.1.1 Representation

Every distinct error name across the whole program is assigned a unique,
nonzero, comptime-known integer tag (the global error namespace is built during
semantic analysis). An error set type is therefore represented at runtime as a
single unsigned integer wide enough to hold its members' tags — in practice a
`u16`. There is:

- no payload attached to an error value,
- no allocation when an error is produced or propagated,
- no implicit context object, message string, or backtrace stored in the value
  itself.

Because error tags are global and stable within a compilation, two error sets
that share a name share the *same* value for that name. `error{NotFound}` from
one module and `error{NotFound}` from another refer to the identical error value;
this is what makes error sets mergeable and subset-comparable (§6.7).

### 6.1.2 The anonymous error set

An error set may be written inline anywhere a type is expected:

```k2
fn parse(text: []const u8) error{ Empty, Overflow }!u32 {
    // ...
}
```

This is an ordinary type expression; it is equivalent to declaring a named
`const` and using it. Naming an error set is purely a readability choice.

### 6.1.3 `anyerror`

`anyerror` is the **open superset** of all error sets: it is the error set
containing every error value the program defines. It is used by code that must
erase the specific set — for example, a generic logging or boundary function that
accepts any error:

```k2
fn logFailure(out: anytype, err: anyerror) !void {
    try out.print("failed: {s}\n", .{@errorName(err)});
}
```

Coercing a specific error set to `anyerror` is always allowed and lossless (it is
a widening). Going the other way — narrowing `anyerror` to a specific set — must
be done explicitly by `switch`ing over the error (any branch you do not handle is
a logic decision you made visible). Prefer specific error sets wherever possible:
they let callers exhaustively handle exactly the failures that can occur (§6.8),
and they let the compiler reject `switch`es that forget a case.

---

## 6.2 Error union types: `E!T` and `!T`

A function that can fail returns an **error union**. The type is written with `!`
between an error set and a result type:

```k2
E!T     // explicit: errors come from set E; success yields a value of type T
!T      // inferred: the error set is computed from the function body (§6.6)
```

The symbolic `!` is reserved **strictly** for error unions in k2. (Logical
negation is the keyword `not`, per the boolean operator set `and` / `or` / `not`;
`!` never means "not".) This keeps a single, unambiguous reading: wherever you
see `!`, a fallible value is in play.

An error union value is, at runtime, a tagged representation that is *either* an
error value of set `E` *or* a value of type `T`. Conceptually:

```
E!T  ≈  one discriminator bit-or-tag  +  ( E  |  T )
```

The success and error variants overlap in storage where the layout permits, so an
error union is typically no larger than `max(sizeOf(E), sizeOf(T))` plus a small
discriminant; for `!void` it collapses to just the error integer. No heap, no
boxing.

A few important cases:

| Type     | Meaning                                                              |
| -------- | ------------------------------------------------------------------- |
| `!void`  | may fail; on success yields nothing. The canonical `main` return.   |
| `!T`     | may fail with an *inferred* error set; on success yields a `T`.     |
| `E!T`    | may fail with errors *exactly from `E`*; on success yields a `T`.   |
| `E!void` | may fail with errors from `E`; on success yields nothing.           |

You cannot accidentally ignore an error union. A bare expression of type `E!T`
used where a `T` is required is a compile error: you must resolve the union with
`try`, `catch`, or an explicit `switch`/`if`-capture.

Crucially, the discard sigil `_` does **not** provide an escape hatch here:
`_ = expr;` on an error-union value is *itself a compile error*. Discarding the
union would silently swallow the possibility of failure — exactly what this rule
exists to prevent — and there is no "success value" to discard on its own, since
the union has not been resolved. (`_ = expr;` remains the ordinary way to discard
a plain, non-union result, as described in §3; it just cannot be used to drop an
unresolved `E!T`.)

To *deliberately* ignore a failure, you must make that choice visible by handling
the error explicitly. The canonical spelling is an empty `catch`:

```k2
// "I have considered this failure and am choosing to ignore it."
expr catch {};
```

`expr catch {}` resolves the union (the empty block handles the error by doing
nothing) and yields the success value, which you may then bind or discard:

```k2
_ = expr catch fallback;           // ignore the error, then discard the success value
const value = expr catch fallback; // ignore the error, keep a value (fallback on error)
```

(For an `E!void` operand the empty `expr catch {};` is the complete idiom: there
is no success value to discard, only the failure to ignore.)

The rule, stated once: ignoring an error is always written, never inferred —
there is no form of `_ = …;` that drops an unhandled error.

The entry point itself is an error union:

```k2
pub fn main(sys: *System) !void { ... }
```

If `main` returns an error, the runtime prints the error name (and, in debug
builds, the error return trace — §6.5) and exits with a nonzero status. This is
the one place the program boundary turns an unhandled error value into a process
exit code; it is not exception propagation, just the top frame inspecting a
returned value.

---

## 6.3 `try`: propagate on error

`try expr` evaluates `expr`, which must be of some error-union type `E!T`:

- if `expr` is the **success** variant, `try` yields the unwrapped `T`;
- if `expr` is the **error** variant, `try` **returns that error from the
  enclosing function**, unchanged.

`try` is the *only* sugar in the error system, and it is deliberately tiny:

```k2
const value = try mayFail(x);
```

expands to exactly:

```k2
const value = mayFail(x) catch |err| return err;
```

That is the whole definition. There is no hidden cost: `try` is a branch and, on
the error path, a `return`. The error set of the enclosing function must be able
to represent every error that any `try` in its body can propagate — with an
explicit `E!T` return type that is checked; with an inferred `!T` return type it
is computed (§6.6).

`try` composes left-to-right with ordinary calls, so a pipeline of fallible steps
reads as a straight line, with each potential early exit visible at the `try`
keyword:

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // Each `try` is a visible early-return point.
    const buf: []u32 = try alloc.alloc(u32, 16);
    defer alloc.free(buf);

    const out = sys.io.stdout();
    try out.print("allocated {d} slots\n", .{buf.len});
}
```

`try` may only appear in a function whose return type is an error union (or in a
comptime/`test` context that is itself an error union); using `try` in a function
that returns a non-union type is a compile error, because there would be nowhere
to return the error to.

---

## 6.4 `catch`: handle on error

`catch` is an *expression* that consumes an error union and produces a plain
value (or transfers control out of the current block). It has two forms.

### 6.4.1 `catch` with a default value

```k2
const port: u16 = parsePort(text) catch 8080;
```

If `parsePort(text)` succeeds, `port` is its success value; if it fails, the error
is discarded and `port` becomes the fallback `8080`. The fallback expression is
evaluated only on the error path.

### 6.4.2 `catch` with a capture block

The capture form binds the error value to a name and runs a block. The block must
either produce a value of the success type or diverge (via `return`, `break`,
`continue`, `unreachable`, or `@panic`):

```k2
const std = @import("std");

const ParseError = error{ Empty, NotANumber };

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // `catch` captures the error value and handles it.
    const ptr = parseDoubled(sys.heap, "21") catch |err| {
        try out.print("parse failed: {s}\n", .{@errorName(err)});
        return;
    };
    defer sys.heap.destroy(ptr);

    try out.print("doubled = {d}\n", .{ptr.*});
}
```

Inside the block, `|err|` has the *exact* error-set type of the operand, so it can
be `switch`ed exhaustively (§6.8). The capture form is how you make a *decision*
about a failure: log it, substitute a value, retry, or convert it into a different
error before re-`return`ing.

`catch` never unwinds a stack and never runs anything implicitly. It is one
branch: success continues; error runs the handler. That is why it costs the same
as an `if`.

### 6.4.3 `try` vs `catch`

- Use **`try`** when this function cannot meaningfully handle the failure and
  should pass it to its caller. (`try x` is `x catch |err| return err`.)
- Use **`catch`** when this function *is* the right place to deal with it — to
  supply a default, to translate the error, or to log and stop.

Both are ordinary, visible constructs. Neither introduces a control-flow channel
the reader cannot see.

---

## 6.5 `errdefer`: cleanup on the error path

`defer stmt` runs `stmt` when its enclosing scope exits, by any path. `errdefer
stmt` is its error-only sibling: it runs **only if the scope is exited by
returning an error** (including an error propagated by `try`). On a normal exit,
or on a `return` of a *success* value, an `errdefer` does **not** run.

This is the standard way to release a *half-built* resource: acquire it, register
its cleanup with `errdefer`, and let later fallible steps trigger that cleanup
automatically if they fail — while the success path keeps the resource and the
caller takes ownership:

```k2
const std = @import("std");

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
    return cell; // success: the errdefer does NOT run; ownership passes out.
}
```

Trace the two outcomes:

- If the loop hits a non-digit, `return ParseError.NotANumber` exits via an error,
  so the `errdefer alloc.destroy(cell)` fires and the half-built cell is freed. No
  leak.
- If parsing succeeds, control reaches `return cell` with a *success* value, the
  `errdefer` is **skipped**, and the live `cell` is handed to the caller, which is
  now responsible for it (the caller pairs it with `defer sys.heap.destroy(ptr)`).

`errdefer` and `defer` are scoped, registered in source order, and run in reverse
order of registration. They are pure data movement plus a branch on the
error/success discriminant — there is no destructor mechanism and nothing runs
implicitly on scope exit *unless you wrote a `defer`/`errdefer`*. This is the
exact discipline named in k2's memory model: cleanup is always written
explicitly.

A capture form is also available when the cleanup needs to inspect which error
occurred:

```k2
errdefer |err| log.record(err);
```

---

## 6.6 Inferred error sets (`!T`)

When a function's return type is written `!T` (with no error set before the `!`),
k2 **infers** the error set from the function body. The inferred set is the union
of:

- every error value the body produces directly (e.g. `return SomeError.X`), and
- the error sets of every expression the body propagates with `try` (or re-raises
  via `catch |e| return e`).

```k2
const std = @import("std");

// Return type `!void` — the error set is inferred to be the union of
// whatever `alloc.alloc`, `out.print`, and any other `try`'d call can fail with.
fn writeReport(alloc: Allocator, out: anytype) !void {
    const line: []u8 = try alloc.alloc(u8, 64);
    defer alloc.free(line);
    try out.print("report ready ({d} bytes)\n", .{line.len});
}
```

Inferred sets are convenient for application code and for functions whose exact
failure modes are an implementation detail. They are computed precisely (not
widened to `anyerror`), so a caller that *does* want to `switch` exhaustively over
the result can still do so — the compiler knows the concrete inferred set at the
call site.

Use an **explicit** `E!T` when the error set is part of your stable API contract
and you want the compiler to reject any new failure mode that creeps into the
body; use **`!T`** when you want the set to track the implementation. Both forms
are zero-cost; the difference is purely about who pins down the set.

> Note: `anyerror!T` is *not* the same as `!T`. `anyerror!T` deliberately erases
> the set to the open superset; `!T` infers the *precise* closed set. Reach for
> `anyerror` only when you truly must forget the specifics.

---

## 6.7 Merging error sets

Error sets compose with the `||` set-union operator. `A || B` is the error set
containing every member of `A` and every member of `B` (duplicates by name
coalesce, since names are globally identified — §6.1.1):

```k2
const IoError    = error{ BrokenPipe, WouldBlock };
const ParseError = error{ Empty, NotANumber };

// The merged set has four members.
const RequestError = IoError || ParseError;

fn handle(...) RequestError!Response {
    // may return any of BrokenPipe, WouldBlock, Empty, NotANumber
}
```

This is exactly the pattern used in the canonical `parseDoubled` above, whose
return type is `(ParseError || error{OutOfMemory})!*u32`: the function can fail
with a parse error *or* with the allocation error, and the merged set names both.

`||` is a **comptime** operation on types; it produces no runtime code. Error sets
are also **subset-comparable** at comptime: `A` coerces to `B` if every member of
`A` is a member of `B`. This is what makes propagation type-check — a function may
`try` a callee whose error set is a subset of (or equal to) the function's own
declared set. Returning an error from a *larger* set than the function declares is
a compile error, naming the offending error value.

Because merging and subset checks are comptime, you can build error-set algebra
into generic code: a function over `comptime T` can compute the precise union of
the error sets of the operations it performs on `T` and declare exactly that.

---

## 6.8 Comptime introspection of error sets

Error sets are types, and like all types they are introspectable at comptime via
`@typeInfo`. Reflecting an error set yields its list of members, so a library can
generate code that is *exhaustive by construction* over the exact errors a callee
can return:

```k2
const std = @import("std");

/// Produce a human-readable label for every error in a set, generated at
/// comptime from the set's own members — no macros, ordinary k2.
fn describe(comptime E: type, out: anytype, err: E) !void {
    const info = @typeInfo(E);
    if (info != .ErrorSet) {
        @compileError("describe requires an error set, got " ++ @typeName(E));
    }
    var index: usize = 0;
    // `inline for` unrolls over the comptime-known error-set members.
    inline for (info.ErrorSet) |e| {
        if (err == @field(E, e.name)) {
            // `e.name` (comptime) and `@errorName(err)` (runtime) agree here.
            try out.print("error[{d}]: {s}\n", .{ index, e.name });
            return;
        }
        index += 1;
    }
    unreachable; // every value of E is one of its members
}
```

Two complementary patterns fall out of this:

1. **Exhaustive `switch` over a captured error.** Because the capture in
   `catch |err|` has the operand's precise set, you can `switch` it and the
   compiler will *force* you to cover every case (or write an explicit `else`):

   ```k2
   const RequestError = error{ BrokenPipe, WouldBlock, Empty, NotANumber };

   fn classify(out: anytype, e: RequestError) !void {
       switch (e) {
           error.BrokenPipe, error.WouldBlock => try out.print("io\n", .{}),
           error.Empty, error.NotANumber       => try out.print("input\n", .{}),
       }
   }
   ```

   If a new member is later added to `RequestError`, this `switch` stops
   compiling until you handle it — failure modes cannot silently slip past you.

2. **Code generation keyed on the set.** Serializers, RPC stubs, and FFI shims can
   walk `@typeInfo(E).ErrorSet` to map each error to a wire code, a C `errno`, or a
   localized message table — all resolved during compilation, emitting no runtime
   reflection and no allocation.

Useful comptime facilities in this space:

- `@typeInfo(E)` → the structured description of `E`; for an error set its
  `.ErrorSet` field is the comptime list of members (each with a `.name`), the
  basis for all error-set metaprogramming.
- `@field(E, name)` → recover the error *value* for a comptime-known member name,
  letting generated code compare against, and dispatch on, each member.
- `@errorName(err)` → the member's name as a `[]const u8`, used above for
  diagnostics and wire encoding.
- `@typeName(E)` → the error set's own name, handy in `@compileError` messages.

All of these are comptime-evaluable where their inputs are comptime-known, so the
reflection itself is free at runtime. (The error model also guarantees each error
value has a stable integer tag — §6.1.1 — which ABI shims map to and from C
`errno` codes; that mapping is generated from `@typeInfo` at compile time.)

---

## 6.9 Error return traces (debug)

An error *value* carries no backtrace — it is a bare integer (§6.1.1). To keep
failures debuggable without paying for that at runtime, k2's **safe build modes
(Debug and ReleaseSafe)** maintain a lightweight, per-thread **error return
trace**: a small ring buffer of the source locations through which an error value
was `return`ed and `try`-propagated. Each time an error first appears
(`return SomeError.X`) and each time `try` re-propagates it, the current location
is appended.

When `main` (or a `test`) finally surfaces an unhandled error, the runtime prints
that trace, so you see the *chain of propagation* that carried the error from its
origin up to the boundary — not a single throw site, but the whole `try` path:

```
error: NotANumber
    parseDoubled (parse.k2:14)   <- where the error originated
    handleRequest (server.k2:88) <- propagated by `try`
    main          (main.k2:7)    <- surfaced here
```

Key properties that keep this honest with k2's pillars:

- **It is not part of the value.** The trace lives in thread-local debug storage,
  not in the `E!T`. Two functions never disagree about what an error *is*; the
  trace is purely diagnostic metadata.
- **It is stripped in `ReleaseFast`.** Trace maintenance is one of the safety
  facilities removed when checks are stripped, so production binaries pay nothing —
  consistent with "native speed, no runtime."
- **It is not exception unwinding.** Recording a location is an array write on the
  normal `return`/`try` path; it does not change control flow, walk frames, or
  allocate. If an error is *handled* by a `catch` partway up, propagation stops and
  so does the trace — exactly mirroring the visible control flow in the source.

The error return trace is a debugging aid bolted onto the *existing* visible
propagation path; it never invents control flow that you cannot read in the code.

---

## 6.10 Contrast with exceptions

It is worth being precise about how k2's model differs from exception-based
languages, because the differences are the point.

| Aspect                 | Exceptions (e.g. C++/Java/Python)                          | k2 errors as values                                              |
| ---------------------- | --------------------------------------------------------- | --------------------------------------------------------------- |
| Control flow           | Hidden: any call may unwind to a distant `catch`.         | Visible: only `try` early-returns, only at the `try` keyword.   |
| In the type system     | Often invisible (unchecked) or coarse (`throws Exception`).| Encoded in the return type as `E!T`; checked and inferable.     |
| Cost when *not* failing | Often "zero-cost" tables, but the happy path is opaque.    | A branch you can see; no tables, no opacity.                    |
| Cost when failing       | Stack unwinding, frame walking, sometimes allocation.     | A compare and a `return`; same cost as any branch.              |
| Cleanup                 | Implicit (destructors / `finally`) firing on unwind.      | Explicit `defer`/`errdefer`; nothing runs unless you wrote it.  |
| Error payload           | Heap-allocated object with message + backtrace.           | A small integer tag; trace (debug only) lives outside the value.|
| Exhaustiveness          | Hard to know which exceptions a call may throw.            | `@typeInfo` + `switch` give compiler-checked exhaustiveness.    |
| Erasure                 | The default; specifics are easily lost.                   | Opt-in via `anyerror`; specific sets are the norm.              |

The single sentence version: **an exception is invisible control flow carrying a
heap object; a k2 error is a visible branch carrying an integer.**

---

## 6.11 Why this is zero-cost and has no hidden control flow

Pulling the threads together:

**Zero-cost.**
An error union `E!T` lowers to a tagged scalar — at most a discriminant plus the
larger of the two variants, and for `!void` just the error integer. Producing an
error is writing that integer; propagating it (`try`) is a branch and a `return`;
handling it (`catch`) is a branch. There is no allocation, no boxing, no RTTI, no
unwind table walk, and no per-error metadata in the value. After
monomorphization, capability indirections and error-set algebra are folded at
comptime, so an `E!T` pipeline compiles to the same compare-and-branch sequence a
careful programmer would write by hand with an out-parameter and a status code.
This is the zero-cost-abstraction requirement applied to failure: the abstraction
is real in the source and absent in the machine code. Safe-build extras (the error
return trace, narrowing checks) are exactly that — *extras* — and are stripped in
`ReleaseFast`.

**No hidden control flow.**
Every non-linear exit in a k2 function comes from the explicit set —
`if`/`else`, `while`, `for`, `switch`, `try`, `catch`, `defer`, `errdefer`,
`orelse`, `return`, `break`, `continue`, `unreachable`. In the error subsystem
that means:

- The only construct that performs an early return on failure is **`try`**, and it
  does so *only* at the textual `try`, expanding to a plain `return err`.
- **`catch`** is an expression that branches once; it never unwinds a stack.
- **`errdefer`** runs only on the error-return path, and only because you wrote it;
  there are no implicit destructors.
- The **error return trace** is diagnostic metadata recorded along the *existing*
  `try`/`return` path; it does not alter control flow.

So you can audit a function's failure behavior the same way you audit its
arithmetic: by reading it. There is no action at a distance, no surprise handler,
and no joule spent on machinery you did not ask for — which is the whole promise
of k2.

---

### See also

- **§5 — Memory management:** `defer`/`errdefer` and the `errdefer`-for-half-built
  -resources idiom in the context of the `Allocator` capability.
- **§7 — Optionals (`?T`, `orelse`):** the sibling mechanism for *absence*, which
  composes with error unions but is a distinct concept.
- **§9 — comptime and reflection:** `@typeInfo`/`@Type`, the general machinery
  behind error-set introspection in §6.8.
