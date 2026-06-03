# 05 — Memory and Allocators

> Part of the **k2** language specification.
> *k2: total control over the machine, with zero waste.*

This chapter specifies k2's memory model: where values live, how pointers
and slices refer to them, how heap memory is obtained through the explicit
`Allocator` capability, which standard allocators ship in `std`, how
`defer`/`errdefer` give you deterministic cleanup, what the ownership
conventions are, and exactly which safety guarantees you get in Debug versus
which checks are stripped in ReleaseFast.

The governing principle of this chapter is the charter pillar **No hidden
allocation**:

> The language and standard library never allocate heap memory on your
> behalf. Any function that may allocate takes an explicit `Allocator`
> capability as a parameter. There is no global allocator, no implicit
> growable-string magic, no autoboxing. If a value lives on the heap, the
> code that put it there is visible at the call site.

Everything below is a consequence of taking that sentence literally.

---

## 1. Overview

k2 has **manual memory management**: no garbage collector, no reference
counting, no language runtime, and no destructor that fires implicitly on
scope exit. There are exactly two places a value can live:

- **The stack** — automatic storage for locals, function frames, and
  fixed-size aggregates. Stack memory is reclaimed when the frame returns.
  It costs nothing to allocate and nothing to free, and no code runs to
  reclaim it.
- **The heap** — dynamic storage obtained *only* through an `Allocator`
  capability that was passed in as a value. The heap is never touched
  without an `Allocator` in scope.

Because the heap is reachable only through a capability value, a function's
signature is an honest account of whether it can allocate. A function that
takes no `Allocator` cannot put anything on the heap, full stop.

---

## 2. The stack

Every `const`/`var` binding whose type has a known, fixed size is a stack
value. Function parameters, locals, fixed arrays, and embedded structs all
live in the frame of the function that declares them.

```k2
const std = @import("std");

const Point = struct { x: f64, y: f64 };

pub fn main(sys: *System) !void {
    // All four of these live on the stack of `main`. No allocator touched.
    var counter: u32 = 0;
    const origin = Point{ .x = 0.0, .y = 0.0 };
    const grid: [4]Point = .{ origin, origin, origin, origin };

    counter += @intCast(grid.len);

    const out = sys.io.stdout();
    try out.print("origin.x = {d}, counter = {d}\n", .{ origin.x, counter });
}
```

Stack values are released automatically when the frame returns — but note
the precise meaning of "released": the *storage* is reclaimed. **No user
code runs.** k2 has no destructors, so if a stack value owns a heap
resource, you must release that resource explicitly (see §6). Returning a
pointer or slice to a stack local is a dangling reference; the storage is
gone the instant the frame returns.

---

## 3. Pointers and slices

k2 distinguishes single-item pointers from slices, and there is **no
implicit null** — absence is modeled with optionals, never with a pointer
that might secretly be zero.

### 3.1 Single-item pointers: `*T` and `*const T`

A `*T` is a pointer to exactly one `T`. A `*const T` points to a `T` you may
read but not write. Dereference with `.*`, take an address with `&`.

```k2
pub fn main(sys: *System) !void {
    var n: u32 = 41;
    const p: *u32 = &n;   // exclusive, mutable pointer to `n`
    p.* += 1;             // write through the pointer

    const r: *const u32 = &n; // shared, read-only view
    const out = sys.io.stdout();
    try out.print("n = {d}\n", .{r.*}); // prints 42
}
```

The `*T` vs `*const T` distinction is also k2's data-race discipline (see
the concurrency chapter): `*T` means *exclusive* access, `*const T` means
*shared* access.

### 3.2 Slices: `[]T` and `[]const T`

A slice is a **pointer plus a length**: a fat pointer that refers to a
contiguous run of elements. `[]T` is mutable; `[]const T` is read-only. A
slice does **not** own its backing memory — it is a *view*. Whoever created
the backing storage owns it and is responsible for freeing it.

```k2
pub fn main(sys: *System) !void {
    var arr = [_]u32{ 10, 20, 30, 40 };
    const whole: []u32 = &arr;        // view over the whole stack array
    const tail: []const u32 = arr[2..]; // read-only view of the last two

    whole[0] = 99; // legal: `whole` is mutable, backs onto `arr`

    const out = sys.io.stdout();
    try out.print("arr[0] = {d}, tail[1] = {d}\n", .{ arr[0], tail[1] });
}
```

In safe builds, slice indexing and slicing are **bounds-checked**: an
out-of-range index triggers an explicit `@panic`. This is a *programmer
mistake*, not an error value — it is never returned as `error{...}` (see
§8 and the error-model chapter).

### 3.3 Optional pointers and the empty slice

Absence of a pointer is `?*T`, unwrapped with `orelse` or capture — never a
magic null. The canonical empty slice is `&.{}` (a slice of length zero),
which is the correct initial value for an owning container that has not yet
allocated.

```k2
fn firstOrNull(items: []const u32) ?*const u32 {
    if (items.len == 0) return null;
    return &items[0];
}
```

---

## 4. The `Allocator` capability

Heap memory comes from an `Allocator`. Per the memory model, an `Allocator`
is **an interface: a struct of function pointers plus a context pointer**.
This is the vtable-like shape, conceptually:

```k2
const std = @import("std");

/// The shape of the Allocator capability (conceptual).
/// `ctx` is the opaque state of the concrete allocator (arena, GPA, ...);
/// the function pointers are its vtable. Call sites never see this directly
/// — they use the methods `alloc` / `free` / `realloc` / `create` / `destroy`.
const Allocator = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    const VTable = struct {
        alloc: fn (ctx: *anyopaque, len: usize, alignment: u8) ?[*]u8,
        resize: fn (ctx: *anyopaque, buf: []u8, new_len: usize) bool,
        free: fn (ctx: *anyopaque, buf: []u8) void,
    };
};
```

Two consequences follow directly from the capability model:

1. **Cost.** A capability is "a plain struct of function pointers plus
   context, so it costs nothing beyond an indirect call and can be
   specialized away by the optimizer when monomorphic." When the concrete
   allocator type is known, LLVM devirtualizes the call entirely.
2. **No globals.** There is no default allocator, no `std.heap.global`,
   nothing ambient. You receive `sys.heap` (an `Allocator`) from the root
   `*System` and thread it explicitly to anything that needs the heap.

### 4.1 The high-level methods

Call sites use these methods, not the raw vtable:

| Method | Signature (conceptual) | Purpose |
| --- | --- | --- |
| `alloc(T, n)` | `(comptime T: type, n: usize) ![]T` | Allocate a slice of `n` elements of `T`. Errors with `OutOfMemory`. |
| `free(slice)` | `(slice: []T) void` | Release a slice previously returned by `alloc`/`realloc`. |
| `realloc(slice, n)` | `(slice: []T, n: usize) ![]T` | Grow or shrink an allocation, preserving contents up to the smaller length. |
| `create(T)` | `(comptime T: type) !*T` | Allocate one `T`, returning a single-item pointer. |
| `destroy(ptr)` | `(ptr: *T) void` | Release a single `T` previously returned by `create`. |

`alloc`/`realloc`/`create` return error unions because they may fail with
`OutOfMemory`; that failure is an ordinary value handled with `try`/`catch`,
not an exception. `free`/`destroy` cannot fail and return `void`.

### 4.2 Allocating a slice

The canonical pattern: allocate, immediately pair with `defer`, then use.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // The heap is a capability; nothing allocates without it.
    const alloc = sys.heap;

    // Allocate a slice of 16 u32s. `try` propagates OutOfMemory.
    const buf: []u32 = try alloc.alloc(u32, 16);
    // Pair every allocation with its release in the same scope.
    defer alloc.free(buf);

    for (buf, 0..) |*slot, i| {
        slot.* = @intCast(i * i);
    }

    const out = sys.io.stdout();
    try out.print("buf[15] = {d}\n", .{buf[15]});
}
```

### 4.3 Allocating a single value

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    const cell: *u32 = try alloc.create(u32);
    defer alloc.destroy(cell);

    cell.* = 42;

    const out = sys.io.stdout();
    try out.print("cell = {d}\n", .{cell.*});
}
```

### 4.4 Growing an allocation with `realloc`

`realloc` is how growable structures expand. It returns a new slice (which
may or may not share the old backing storage); after a successful
`realloc`, the **old slice is invalid** and only the returned slice may be
used. The charter's `List` is the canonical example:

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
    var nums = List(u32).init(sys.heap);
    defer nums.deinit();
    try nums.push(40);
    try nums.push(2);
    const out = sys.io.stdout();
    try out.print("sum = {d}\n", .{nums.items[0] + nums.items[1]});
}
```

Note the lifetime contract embedded in this type: `init` stores the
`Allocator` it was handed, every `push` uses *that* allocator, and `deinit`
frees with *that same* allocator. A container and its allocator are bound
together for the container's whole life. Never `free` a container's storage
with a different allocator than the one that created it — in Debug this is
detected and panics; in ReleaseFast it is undefined behavior.

---

## 5. Standard allocators

`std` ships a family of allocators. They all expose the same `Allocator`
interface, so any code written against `Allocator` works with any of them —
this is the whole point of the capability being an interface. You choose a
*strategy* at the point where you create the allocator, and the rest of the
program is agnostic.

### 5.1 Page allocator — `std.heap.PageAllocator`

Requests memory directly from the OS in page-sized units. It is the coarse,
bottom-of-the-stack allocator: cheap to reason about, expensive per call,
no per-allocation bookkeeping. Use it as the *backing* allocator for an
arena or fixed-buffer allocator, or for a handful of large, long-lived
allocations.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    var page = std.heap.PageAllocator.init(sys);
    const alloc = page.allocator();

    const big: []u8 = try alloc.alloc(u8, 1 << 20); // 1 MiB straight from the OS
    defer alloc.free(big);

    big[0] = 7;
    const out = sys.io.stdout();
    try out.print("big[0] = {d}\n", .{big[0]});
}
```

### 5.2 Arena allocator — `std.heap.ArenaAllocator`

An arena allocates from a backing allocator and **never frees individual
allocations**; instead, *everything* is freed at once when the arena is
deinitialized. This makes per-object `free` calls unnecessary and turns
cleanup into a single operation — ideal for request-scoped or
phase-scoped work where many objects share one lifetime.

```k2
const std = @import("std");

/// Build a scratch buffer of `n` doubled values; the caller does not free
/// each piece — the arena frees the whole region at once.
fn fillScratch(alloc: Allocator, n: usize) ![]u32 {
    const xs: []u32 = try alloc.alloc(u32, n);
    for (xs, 0..) |*slot, i| {
        slot.* = @intCast(i * 2);
    }
    return xs;
}

pub fn main(sys: *System) !void {
    // The arena is backed by the heap capability.
    var arena = std.heap.ArenaAllocator.init(sys.heap);
    // One defer reclaims EVERYTHING allocated through the arena.
    defer arena.deinit();
    const alloc = arena.allocator();

    const a = try fillScratch(alloc, 4);
    const b = try fillScratch(alloc, 8);
    // No individual frees: that is the arena's contract.

    const out = sys.io.stdout();
    try out.print("a[3] = {d}, b[7] = {d}\n", .{ a[3], b[7] });
}
```

### 5.3 Fixed-buffer allocator — `std.heap.FixedBufferAllocator`

Hands out memory from a **caller-provided byte buffer**, with zero heap
involvement. When the buffer is exhausted, allocation fails with
`OutOfMemory`. This is the allocator for hard real-time and embedded code,
and for comptime-bounded work where you want a provable upper bound on
memory and no OS interaction at all.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // Backing storage lives on the stack — no heap, no capability needed
    // to allocate from it once it exists.
    var storage: [256]u8 = undefined;
    var fba = std.heap.FixedBufferAllocator.init(&storage);
    const alloc = fba.allocator();

    const a: []u32 = try alloc.alloc(u32, 8);   // 32 bytes of `storage`
    defer alloc.free(a);

    a[0] = 123;
    const out = sys.io.stdout();
    try out.print("a[0] = {d}\n", .{a[0]});
}
```

### 5.4 General-purpose allocator — `std.heap.GeneralPurposeAllocator`

The GPA is the default-quality, fully general allocator and the one you
reach for when no specialized strategy applies. In **safe builds (Debug and
ReleaseSafe)** it is the runtime-checked allocator that "detects leaks,
double-frees, and use-after-free." In ReleaseFast those checks are stripped
for raw speed.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // In Debug/ReleaseSafe this GPA tracks every allocation.
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    // On deinit, the GPA reports any allocation that was never freed.
    defer {
        const leaked = gpa.deinit();
        if (leaked) @panic("memory leak detected");
    }
    const alloc = gpa.allocator();

    const buf: []u8 = try alloc.alloc(u8, 64);
    defer alloc.free(buf); // remove this line and Debug reports a leak

    buf[0] = 1;
    const out = sys.io.stdout();
    try out.print("buf[0] = {d}\n", .{buf[0]});
}
```

> **Tip.** A common composition is a GPA as the program-wide backing
> allocator, with short-lived arenas carved out of it for per-request work.
> You get leak detection at the boundary *and* cheap bulk cleanup inside.

---

## 6. Deterministic cleanup: `defer` and `errdefer`

Because nothing runs implicitly on scope exit, k2 gives you two explicit,
visible primitives for cleanup. Both are part of the charter's fixed set of
non-linear control flow.

### 6.1 `defer` — always run on scope exit

`defer stmt;` schedules `stmt` to run when the enclosing scope exits **by
any path** — normal fall-through, `return`, `break`, or an error
propagating through. Multiple `defer`s run in **last-in, first-out** order,
which makes nested resources unwind in the correct sequence.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    const first: []u8 = try alloc.alloc(u8, 8);
    defer alloc.free(first);   // runs second

    const second: []u8 = try alloc.alloc(u8, 8);
    defer alloc.free(second);  // runs first (LIFO)

    first[0] = 1;
    second[0] = 2;

    const out = sys.io.stdout();
    try out.print("{d} {d}\n", .{ first[0], second[0] });
    // On return: free(second) then free(first).
}
```

The idiomatic rule is: **pair every allocation with its release in the same
scope.** Writing the `defer` on the line immediately after the allocation
makes the lifetime obvious and local.

### 6.2 `errdefer` — run only on the error path

`errdefer stmt;` schedules `stmt` to run **only if** the scope exits via an
error. This is the standard way to release a half-built resource: you
`errdefer` the cleanup right after acquiring the resource, and if a later
step fails, the partial work is unwound — but on the success path the
resource survives to be returned to the caller. The charter's
`parseDoubled` is the canonical example:

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

Trace the two paths through `parseDoubled`:

- **Error path** (e.g. `text = "1x"`): `create` succeeds, then the loop hits
  a non-digit and `return ParseError.NotANumber` fires. The `errdefer`
  runs, `destroy(cell)` releases the half-built cell, and no memory leaks.
- **Success path**: control reaches `return cell`. Because the function
  returns *without* an error, the `errdefer` does **not** run — ownership of
  `cell` transfers to the caller, who pairs it with its own
  `defer sys.heap.destroy(ptr)`.

This is the precise division of labor: `defer` for resources you keep for
the whole scope, `errdefer` for resources you are *building to hand off* and
must unwind only if the hand-off never happens.

---

## 7. Ownership conventions — who frees what

k2 has no automatic ownership tracking and no borrow checker; ownership is a
**documented convention**, enforced at runtime by the safety allocator in
Debug. The conventions below are the idioms the standard library follows and
that your code should follow too.

### 7.1 The allocating function does not free; the caller does

A function that returns heap memory transfers **ownership** to its caller.
The caller is responsible for freeing it, **with the same allocator** that
created it. Always document the allocator and the matching free in the doc
comment.

```k2
const std = @import("std");

/// Returns a heap-owned copy of `src`. OWNERSHIP: the caller owns the
/// returned slice and must free it with the SAME `alloc`.
fn dupe(alloc: Allocator, src: []const u8) ![]u8 {
    const out: []u8 = try alloc.alloc(u8, src.len);
    errdefer alloc.free(out); // unwind if a later step could fail
    for (src, 0..) |b, i| out[i] = b;
    return out;
}

pub fn main(sys: *System) !void {
    const alloc = sys.heap;
    const copy = try dupe(alloc, "k2");
    defer alloc.free(copy); // caller honors the ownership contract

    const out = sys.io.stdout();
    try out.print("{s}\n", .{copy});
}
```

### 7.2 If you take an `Allocator`, you own the result with it

Store the allocator alongside the data it allocated when the data outlives a
single call. This is exactly what `List` does: it keeps `alloc` as a field
so `deinit` can free with the right one. The rule generalizes: **the
allocator that created a resource is the allocator that must free it.**

### 7.3 `init` / `deinit` pairs

Containers expose `init(alloc)` to acquire and `deinit()` to release. The
caller pairs them with `defer` at the point of creation:

```k2
var list = List(u32).init(sys.heap);
defer list.deinit();
```

`deinit` takes `*Self` and is the *only* place the container frees its
storage. After `deinit`, the container is dead; using it is use-after-free
(caught in Debug).

### 7.4 Borrowed views do not get freed

A `[]const T` or `*const T` parameter is a **borrow**: the callee may read
it for the duration of the call but must **not** free it and must **not**
retain it past the call. Only the owner frees. This is why `dupe` above
takes `src: []const u8` (borrowed) and returns `[]u8` (owned).

---

## 8. Debug-mode safety vs release-mode performance

k2's build modes trade safety checking against raw throughput, all from one
shared MIR. The memory-relevant guarantees per mode:

| Build mode | Backend | Bounds / overflow / narrowing checks | Leak / double-free / use-after-free detection |
| --- | --- | --- | --- |
| **Debug** | Cranelift (fast builds) | On | On (via GeneralPurposeAllocator) |
| **ReleaseSafe** | LLVM (optimized) | On | On (via GeneralPurposeAllocator) |
| **ReleaseFast** | LLVM (optimized) | **Stripped** | **Stripped** |

### 8.1 What the safety allocator catches in safe builds

In Debug and ReleaseSafe, the runtime-checked `GeneralPurposeAllocator`
detects:

- **Leaks** — memory allocated but never freed by the time the GPA is
  deinitialized. `gpa.deinit()` reports it.
- **Double-free** — freeing the same allocation twice triggers `@panic`.
- **Use-after-free** — reading or writing memory after it was freed triggers
  `@panic`.

In addition, the safety-check insertion pass emits **bounds checks** on
slice/array access, **overflow checks** on integer arithmetic, and
**narrowing-cast checks** for `@intCast`. When any of these fail it is a
**programmer mistake**, not an error value: it triggers an explicit
`@panic`. Mistakes are never modeled as `error{...}` and never caught with
`catch`.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;
    const buf: []u32 = try alloc.alloc(u32, 4);
    defer alloc.free(buf);

    var i: usize = 0;
    // In Debug/ReleaseSafe, indexing buf[4] would @panic on the bounds
    // check. In ReleaseFast the check is gone and it is undefined behavior.
    while (i < buf.len) : (i += 1) {
        buf[i] = @intCast(i);
    }

    const out = sys.io.stdout();
    try out.print("buf[3] = {d}\n", .{buf[3]});
}
```

### 8.2 What ReleaseFast strips

ReleaseFast "strips these checks for raw speed." There is no bounds check,
no overflow check, no narrowing-cast check, and the GPA's leak/UAF
bookkeeping is gone. A violated assumption — an out-of-bounds index, a
use-after-free, a `@intCast` that loses bits — is **undefined behavior**,
not a panic. ReleaseFast is for code you have already validated under
Debug/ReleaseSafe and now want to run at full speed.

The discipline this implies: **develop and test in Debug/ReleaseSafe, where
the allocator and safety checks will catch your mistakes; ship ReleaseFast
only after the safe builds are clean.** Because all three modes lower from
the same monomorphized MIR, a program that is correct in ReleaseSafe and
free of undefined behavior is correct in ReleaseFast — the semantics are
identical; only the checks differ.

### 8.3 Comptime leak/escape analysis

Independently of runtime tooling, the compiler runs **comptime leak/escape
heuristics** (a distinctive feature of k2): obvious allocator misuse — a
value allocated and returned without an ownership-transfer signature, or a
missing paired `free` in a simple scope — is surfaced as a **compile-time
diagnostic** rather than waiting for a runtime panic. This catches the
easy classes of leaks before the program ever runs, complementing the
runtime GPA which catches the rest in Debug/ReleaseSafe.

---

## 9. Worked example: putting it together

A complete program demonstrating explicit allocator passing, ownership
transfer across a function boundary, `errdefer` for half-built cleanup,
`defer` for caller-side release, and a leak-checking GPA at the root.

```k2
const std = @import("std");

/// Read `count` squares into a freshly allocated slice.
/// OWNERSHIP: the caller owns the returned slice and frees it with `alloc`.
fn squares(alloc: Allocator, count: usize) ![]u64 {
    const xs: []u64 = try alloc.alloc(u64, count);
    // If anything below could fail after this point, unwind the allocation.
    errdefer alloc.free(xs);

    for (xs, 0..) |*slot, i| {
        const v: u64 = @intCast(i);
        slot.* = v * v;
    }
    return xs; // success: ownership transfers, errdefer does NOT run
}

pub fn main(sys: *System) !void {
    // Root allocator is a leak-checking GPA in safe builds.
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer {
        const leaked = gpa.deinit();
        if (leaked) @panic("leak detected at shutdown");
    }
    const alloc = gpa.allocator();

    const out = sys.io.stdout();

    // Acquire an owned slice; honor the ownership contract with `defer`.
    const data = squares(alloc, 8) catch |err| {
        try out.print("alloc failed: {s}\n", .{@errorName(err)});
        return;
    };
    defer alloc.free(data);

    var total: u64 = 0;
    for (data) |x| total += x;

    try out.print("sum of first 8 squares = {d}\n", .{total});
}
```

What to notice:

- The **only** source of heap power is `sys`, from which the GPA is built
  and `alloc` is derived. Nothing allocates without `alloc` in hand.
- `squares` takes the allocator explicitly; its signature tells you it can
  allocate, and its doc comment states the ownership contract.
- `errdefer` covers the half-built slice inside `squares`; `defer` covers
  the finished, owned slice inside `main`. Exactly one of them frees `data`
  on any given run, never zero and never two.
- The GPA's `deinit` turns any forgotten `free` into a loud, deterministic
  failure in Debug/ReleaseSafe — and the comptime escape analysis would
  flag the simplest leaks even earlier.

---

## 10. Summary

- Values live on the **stack** (automatic, no code runs to reclaim) or the
  **heap** (only via an explicit `Allocator` capability).
- **Pointers** are `*T`/`*const T`; **slices** are `[]T`/`[]const T`, a
  pointer plus length and a non-owning view. There is no implicit null —
  use `?T`.
- `Allocator` is an **interface** (vtable + context pointer) obtained from
  `sys.heap`; call sites use `alloc`/`free`/`realloc`/`create`/`destroy`.
- `std` ships **page**, **arena**, **fixed-buffer**, and
  **general-purpose** allocators; all share the `Allocator` interface so
  code is strategy-agnostic.
- **`defer`** releases on every exit (LIFO); **`errdefer`** releases only on
  the error path — the idiom for unwinding a half-built resource you intend
  to hand off.
- **Ownership is a convention**: the allocator that created a resource frees
  it, allocating functions transfer ownership to the caller, and borrowed
  `const` views are never freed by the borrower.
- **Debug/ReleaseSafe** give leak / double-free / use-after-free detection
  plus bounds/overflow/narrowing checks; **ReleaseFast** strips them for raw
  speed, making violated assumptions undefined behavior. Develop safe, ship
  fast.

This is the memory model in one sentence: **every allocation is visible,
every free is written by you, and the compiler and safe-build allocator are
there to prove you got it right.**
