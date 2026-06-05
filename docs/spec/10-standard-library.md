# 10 — The Standard Library

> Part of the **k2** language specification.
> *k2: total control over the machine, with zero waste.*

This chapter specifies the surface and organization of k2's standard
library, reached as `const std = @import("std");`. It covers the low-level
memory module (`std.mem` and the `Allocator` interface), the
allocator-taking containers (`ArrayList`, `HashMap`, and friends), the
capability-based formatting and I/O facilities (`std.fmt`, `std.io`), the
pure computational modules (`std.math`, `std.sort`, `std.hash`), and the
capability **types** themselves — `Allocator`, `Io`, `Clock`, `Random`,
`Fs`, `Net`, and `Env` — that flow from the root `*System`.

The single rule that organizes the entire library is the charter pillar
**No ambient authority**, extended from its narrower sibling **No hidden
allocation**:

> I/O, the clock, randomness, the environment, and the filesystem are
> capability values that must be passed explicitly from `main` downward.
> There is no `std.print` that secretly grabs stdout, no global rng. Code
> can only do what it was handed the capability to do.

Everything below is a consequence of taking those two sentences literally.
**Nothing in `std` allocates heap memory, and nothing in `std` touches the
outside world, without an explicit capability passed in as a value.** If a
function can do either, you can see it in its parameter list.

---

## 1. Overview and organization

`std` is one module, imported once and threaded through your program as a
comptime namespace value:

```k2
const std = @import("std");
```

Its contents fall into exactly three categories, and the category a thing
belongs to is determined by what authority it needs:

1. **Pure modules** — code that computes and nothing else. `std.math`,
   `std.sort`, `std.hash`, `std.ascii`, `std.unicode`, `std.fmt`, and the
   `std.mem` byte routines neither allocate nor perform I/O. They take no
   capability because they need none; their signatures prove it.
2. **Allocator-taking facilities** — the containers (`ArrayList`,
   `HashMap`, `HashSet`, `BoundedArray`, `ArrayHashMap`) and the few
   routines that build heap values. Every one of them takes an `Allocator`
   as an explicit parameter or constructor argument. They can allocate;
   they cannot do anything else.
3. **Capability types** — the structs that grant authority over the outside
   world: `Allocator`, `Io`, `Clock`, `Random`, `Fs`, `Net`, `Env`. These
   are not obtained by importing `std`; they are obtained from the root
   `*System` handed to `main`, and passed downward as values.

This three-way split is the whole library's shape. A function's signature
tells you which category of power it draws on: take no capability and you
are pure; take an `Allocator` and you may touch the heap and nothing else;
take a `Clock` and you may read time and nothing else. There is no fourth
category of "ambient" facility that reaches the OS without a value in hand,
because k2 has no such facility.

> **The honest-signature rule.** A k2 function is exactly as powerful as the
> capabilities in its parameter list, no more. `fn f(alloc: Allocator)`
> cannot read the clock, open a file, or print; `fn g(x: u32) u32` cannot
> even allocate. You audit what a function *can do* by reading its
> signature, and you audit what it *does* by reading its body. The standard
> library is built so that this rule never has an exception.

---

## 2. `std.mem` and the `Allocator` interface

`std.mem` is the low-level memory module. It is home to the `Allocator`
interface type and to a family of pure routines over slices and bytes. The
`Allocator` interface is specified in full in **§05 — Memory and
allocators**; this chapter *references* it and does not redefine its shape.

### 2.1 The `Allocator` interface, by reference

Per §05, an `Allocator` is **an interface: a struct of function pointers
plus a context pointer** (vtable + `ctx`). Call sites never touch the
vtable; they use these methods, whose contracts are fixed in §05:

| Method | Signature (conceptual) | Purpose |
| --- | --- | --- |
| `alloc(T, n)` | `(comptime T: type, n: usize) ![]T` | Allocate a slice of `n` elements of `T`. Fails with `OutOfMemory`. |
| `free(slice)` | `(slice: []T) void` | Release a slice from `alloc`/`realloc`. Cannot fail. |
| `realloc(slice, n)` | `(slice: []T, n: usize) ![]T` | Grow/shrink, preserving contents up to the smaller length. |
| `create(T)` | `(comptime T: type) !*T` | Allocate one `T`, returning a single-item pointer. |
| `destroy(ptr)` | `(ptr: *T) void` | Release one `T` from `create`. Cannot fail. |

`alloc`/`realloc`/`create` return error unions because they may fail with
`OutOfMemory`, an ordinary value handled with `try`/`catch`. `free` and
`destroy` return `void`. The allocator that created a resource is the
allocator that must free it — never mix allocators on one allocation.

You obtain an `Allocator` from `sys.heap` (see §10), or by building one of
the `std.heap` strategies (page, arena, fixed-buffer, general-purpose) over
a backing allocator, exactly as §05 §5 specifies. Nothing in this chapter
contradicts that API.

### 2.2 Alignment-aware allocation

Most allocations use the natural alignment of `T`. When you need a stronger
alignment — for SIMD lanes, DMA buffers, or cache-line isolation — use the
aligned variant. The requested alignment is a comptime-known power of two,
checked with `@alignOf` against the element type.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // 64-byte (cache-line) aligned slice of f32 lanes.
    const lanes: []align(64) f32 = try alloc.allocAligned(f32, 16, 64);
    defer alloc.free(lanes);

    for (lanes, 0..) |*lane, i| lane.* = @as(f32, @intCast(i));

    const out = sys.io.stdout();
    try out.print("lanes[15] = {d}\n", .{lanes[15]});
}
```

`allocAligned(T, n, alignment)` is the only allocation routine that takes an
explicit alignment; the rest infer it from `@alignOf(T)`. There is no hidden
over-alignment and no padding you did not ask for.

### 2.3 Pure byte and slice routines

`std.mem` also exposes a set of routines that operate on slices you already
own. **None of them allocate** — they take their destination as an explicit
buffer or operate in place. They take no `Allocator` and no capability.

| Routine | Signature (conceptual) | Effect |
| --- | --- | --- |
| `copy(T, dst, src)` | `(comptime T: type, dst: []T, src: []const T) void` | Copy `src` into `dst` (lengths must match; in-place, no allocation). |
| `set(T, dst, value)` | `(comptime T: type, dst: []T, value: T) void` | Fill `dst` with `value`. |
| `eql(T, a, b)` | `(comptime T: type, a, b: []const T) bool` | Element-wise equality. |
| `indexOf(T, hay, needle)` | `(comptime T: type, hay, needle: []const T) ?usize` | First match position, or `null`. |
| `startsWith(T, s, prefix)` | `(comptime T: type, s, prefix: []const T) bool` | Prefix test. |
| `split(T, s, sep)` | `(comptime T: type, s, sep: []const T) Iterator` | A *lazy* iterator over subslices — yields views into `s`, allocates nothing. |
| `trim(T, s, set)` | `(comptime T: type, s: []const T, set: []const T) []const T` | A trimmed **view** (subslice), not a copy. |

The pattern is deliberate: routines that *produce* a new run of bytes
(`dupe`, `join`, `concat`, `replaceOwned`) take an `Allocator` and live in
the allocator-taking category; routines that *inspect* or *view* existing
bytes (`eql`, `indexOf`, `split`, `trim`) take no allocator and return
slices that borrow their input.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const greeting = "  k2 rocks  ";
    // `trim` returns a VIEW into `greeting`; no allocation, nothing to free.
    const core = std.mem.trim(u8, greeting, " ");

    const out = sys.io.stdout();
    try out.print("[{s}] has len {d}\n", .{ core, core.len });
}
```

### 2.4 The two routines that *do* allocate: `dupe` and `join`

`dupe` and `join` are the `std.mem` members that build new heap values, so
they take an `Allocator` and transfer ownership to the caller, exactly per
the §05 ownership conventions.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // `dupe` allocates a heap copy. OWNERSHIP: caller frees with `alloc`.
    const owned = try std.mem.dupe(u8, alloc, "persisted");
    defer alloc.free(owned);

    // `join` concatenates with a separator into a freshly allocated slice.
    const parts = [_][]const u8{ "a", "b", "c" };
    const csv = try std.mem.join(u8, alloc, ",", &parts);
    defer alloc.free(csv);

    const out = sys.io.stdout();
    try out.print("{s} / {s}\n", .{ owned, csv });
}
```

The contrast between §2.3 and §2.4 is the whole library in miniature: the
ones that view take nothing; the ones that build take an allocator.

---

## 3. Allocator-taking containers

k2's containers all follow one shape, dictated by **No hidden allocation**:

- **Every constructor takes an `Allocator`** (or, for the bounded
  containers, needs none because they live in fixed inline storage).
- The container **stores** the allocator it was handed and uses *that same*
  allocator for every growth and for `deinit`.
- **`init(alloc)` acquires; `deinit()` releases**, and the caller pairs them
  with `defer` at the point of creation.
- Methods that may grow the backing storage return an error union (they can
  fail with `OutOfMemory`); methods that cannot grow return plain values.

There is no growable container that secretly grabs a default allocator,
because there is no default allocator. The container's lifetime contract is
the §05 contract: the allocator that created it frees it.

### 3.1 `ArrayList(T)` — the growable sequence

`ArrayList(T)` is the canonical dynamic array and the workhorse container.
`ArrayList` is a comptime function from a type to a type (generics are
functions; §07), `init` takes the `Allocator`, and `deinit` frees with it.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // The constructor takes the allocator; nothing grows without it.
    var list = std.ArrayList(u32).init(alloc);
    // Pair init with deinit at the point of creation.
    defer list.deinit();

    var i: u32 = 0;
    while (i < 8) : (i += 1) {
        // `append` may grow the backing storage, so it can fail with
        // OutOfMemory; `try` propagates that error value.
        try list.append(i * 10);
    }

    const out = sys.io.stdout();
    try out.print("len = {d}, last = {d}\n", .{ list.items.len, list.items[7] });
}
```

The surface (each method's allocator-need is visible in whether it returns
an error union):

| Method | Signature (conceptual) | Allocates? |
| --- | --- | --- |
| `init(alloc)` | `(alloc: Allocator) Self` | Stores `alloc`; no allocation yet. |
| `deinit()` | `(self: *Self) void` | Frees backing storage with the stored allocator. |
| `append(x)` | `(self: *Self, x: T) !void` | May grow → returns `!void`. |
| `appendSlice(xs)` | `(self: *Self, xs: []const T) !void` | May grow → `!void`. |
| `ensureCapacity(n)` | `(self: *Self, n: usize) !void` | May grow → `!void`. |
| `pop()` | `(self: *Self) ?T` | Never grows → plain `?T`. |
| `items` | `[]T` field | The live view; borrowed, never freed by you. |
| `toOwnedSlice()` | `(self: *Self) ![]T` | Hands the buffer to the caller; the list is reset, caller now owns and frees the slice. |

`list.items` is a borrowed `[]T` view valid until the next growth; do not
retain it across an `append`. `toOwnedSlice()` is how you *extract* the
buffer and transfer ownership out of the list — after it, the caller frees
the returned slice with the same allocator, and the list is empty again.

### 3.2 `HashMap(K, V)` and `AutoHashMap(K, V)`

`HashMap(K, V, Context)` is the general unordered map; its `Context` supplies
`hash` and `eql` at comptime. `AutoHashMap(K, V)` is the common case: it
derives `hash`/`eql` automatically from `K` via `@typeInfo` reflection, so
most code uses `AutoHashMap`. Both take the `Allocator` in `init` and free
with it in `deinit`.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // AutoHashMap derives hash/eql from the key type at comptime.
    var counts = std.AutoHashMap([]const u8, u32).init(alloc);
    defer counts.deinit();

    const words = [_][]const u8{ "a", "b", "a", "c", "a", "b" };
    for (words) |w| {
        // `getOrPut` may grow the table → returns an error union.
        const entry = try counts.getOrPut(w);
        if (entry.found_existing) {
            entry.value_ptr.* += 1;
        } else {
            entry.value_ptr.* = 1;
        }
    }

    const out = sys.io.stdout();
    try out.print("count(\"a\") = {d}\n", .{counts.get("a").?});
}
```

The surface (insertion may grow, lookup does not):

| Method | Signature (conceptual) | Allocates? |
| --- | --- | --- |
| `init(alloc)` | `(alloc: Allocator) Self` | Stores `alloc`; empty. |
| `deinit()` | `(self: *Self) void` | Frees the table. |
| `put(k, v)` | `(self: *Self, k: K, v: V) !void` | May grow → `!void`. |
| `getOrPut(k)` | `(self: *Self, k: K) !Entry` | May grow → `!Entry`. |
| `get(k)` | `(self: *Self, k: K) ?V` | Pure lookup → `?V`. |
| `getPtr(k)` | `(self: *Self, k: K) ?*V` | Pure lookup → `?*V`. |
| `remove(k)` | `(self: *Self, k: K) bool` | No allocation → `bool`. |
| `count()` | `(self: *Self) usize` | No allocation → `usize`. |
| `iterator()` | `(self: *Self) Iterator` | A view over entries; allocates nothing. |

When `K` or `V` is itself heap-owned (e.g. `[]const u8` keys you allocated),
the map does **not** own those bytes — it stores the slices you handed it.
Freeing the keys/values is your responsibility, on the same allocator that
created them; `deinit` frees only the table's own backing storage. This is
the §05 borrow-vs-own distinction applied to map contents.

### 3.3 `HashSet(T)`

`HashSet(T)` is the set built on the same machinery: membership without
values. It takes an `Allocator` in `init`, grows on insert, and frees in
`deinit`.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    var seen = std.HashSet(u32).init(alloc);
    defer seen.deinit();

    const xs = [_]u32{ 3, 1, 4, 1, 5, 9, 2, 6, 5, 3 };
    var unique: usize = 0;
    for (xs) |x| {
        // `insert` returns whether the value was newly added; it may grow.
        if (try seen.insert(x)) unique += 1;
    }

    const out = sys.io.stdout();
    try out.print("{d} unique values\n", .{unique});
}
```

| Method | Signature (conceptual) | Allocates? |
| --- | --- | --- |
| `init(alloc)` | `(alloc: Allocator) Self` | Stores `alloc`; empty. |
| `deinit()` | `(self: *Self) void` | Frees the set. |
| `insert(x)` | `(self: *Self, x: T) !bool` | May grow → `!bool` (true if newly added). |
| `contains(x)` | `(self: *Self, x: T) bool` | Pure → `bool`. |
| `remove(x)` | `(self: *Self, x: T) bool` | No allocation → `bool`. |

### 3.4 `BoundedArray(T, N)` — fixed capacity, no allocator

`BoundedArray(T, N)` is the container for code that must not touch the heap
at all: it carries its storage **inline** as a `[N]T` plus a length, so it
needs **no allocator** and has **no `deinit`**. It is the container of
choice for embedded, real-time, and comptime-bounded work, and it is the
proof that the allocator-taking convention is not an accident — when no heap
is involved, no allocator appears.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // Storage is inline; no allocator, no deinit, no heap.
    var buf = std.BoundedArray(u8, 16).init(0);

    // `append` fails with `Overflow` (NOT OutOfMemory) when N is exceeded —
    // a fixed bound, not a heap exhaustion.
    try buf.append('k');
    try buf.append('2');

    const out = sys.io.stdout();
    try out.print("contents = {s}, len = {d}\n", .{ buf.slice(), buf.len });
}
```

Because the capacity `N` is a comptime constant, `BoundedArray`'s entire
footprint is known at compile time. `append` returns `error{Overflow}!void`:
the failure mode is "the fixed bound was hit," never "the heap ran out,"
because there is no heap here at all. `slice()` returns a borrowed view over
the live prefix.

### 3.5 `ArrayHashMap(K, V)` — insertion-ordered

`ArrayHashMap(K, V)` is the map that preserves **insertion order**: it pairs
a hash index with a backing `ArrayList` of entries, so iteration yields keys
in the order they were inserted. It takes the `Allocator` in `init` like the
other maps and frees both its index and its entry array in `deinit`.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    var ordered = std.ArrayHashMap([]const u8, u32).init(alloc);
    defer ordered.deinit();

    try ordered.put("first", 1);
    try ordered.put("second", 2);
    try ordered.put("third", 3);

    const out = sys.io.stdout();
    var it = ordered.iterator(); // yields entries in INSERTION order
    while (it.next()) |entry| {
        try out.print("{s} = {d}\n", .{ entry.key_ptr.*, entry.value_ptr.* });
    }
}
```

Use `AutoHashMap` when you only need lookup; use `ArrayHashMap` when stable
iteration order matters (config files, deterministic output). Both obey the
same allocator-taking, `init`/`deinit` contract.

### 3.6 The container contract in one line

Every container in this section shares one invariant, which is just §05's
ownership rule applied uniformly:

> **A container is bound to the allocator it was `init`'d with for its whole
> life. That allocator grows it, and that allocator — via `deinit` — frees
> it. Never free a container's storage with a different allocator.** In
> Debug/ReleaseSafe a mismatch is detected and panics; in ReleaseFast it is
> undefined behavior.

`BoundedArray` is the lone exception that proves the rule: it has no
allocator and no `deinit` because it never touches the heap.

---

## 4. Capability-based I/O: `std.io`

`std.io` defines the **shapes** of byte streams — the `Writer` and `Reader`
interfaces — and pure helpers over them. What `std.io` deliberately does
**not** contain is any way to *get* a stream that talks to the real world.
There is **no `std.io.stdout`, no `std.io.stderr`, no global console**. A
real stream is a capability, and capabilities come from `sys.io` (an `Io`,
§10), never from importing a module.

This is **No ambient authority** at its sharpest: importing `std` grants you
the *vocabulary* of I/O (the interfaces) but not the *authority* to perform
it (a concrete stream). Authority arrives only as a value passed from
`main`.

### 4.1 The `Writer` interface

A `Writer` is an interface — a context pointer plus a vtable — over anything
that can accept bytes: a file, a socket, an in-memory buffer, a test sink.
Its one primitive is `writeAll`; everything else (`print`, `writeByte`) is
built on it.

```k2
const std = @import("std");

/// The shape of the Writer capability (conceptual). Like `Allocator`, it is
/// a struct of function pointers plus context; call sites use the methods.
const Writer = struct {
    ctx: *anyopaque,
    writeAllFn: fn (ctx: *anyopaque, bytes: []const u8) WriteError!void,

    /// The one primitive: push bytes at the underlying sink.
    pub fn writeAll(self: Writer, bytes: []const u8) WriteError!void {
        return self.writeAllFn(self.ctx, bytes);
    }

    /// Formatted output is built on `writeAll` and performs NO allocation:
    /// it formats directly into the sink. (See §5, `std.fmt`.)
    pub fn print(self: Writer, comptime fmt: []const u8, args: anytype) WriteError!void {
        return std.fmt.format(self, fmt, args);
    }
};

const WriteError = error{ BrokenPipe, DiskFull, Unexpected };
```

The crucial property: `Writer.print` formats *into* the writer and never
allocates. There is no temporary heap buffer behind your back; bytes flow
straight from `std.fmt` into the sink. This is why every `print` call you
have seen in this specification — `out.print("...", .{...})` — needs no
allocator. The writer *is* the destination.

### 4.2 Getting a real `Writer`: it comes from `sys.io`

The only way to obtain a writer that reaches the terminal is through the
`Io` capability on `sys`. There is no other door.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // No ambient stdout: we take the writer FROM the io capability.
    const out = sys.io.stdout(); // a Writer over standard output
    const err = sys.io.stderr(); // a Writer over standard error

    try out.print("normal output: {d}\n", .{42});
    try err.print("diagnostic: {s}\n", .{"to stderr"});
}
```

A function that wants to print must therefore *take a writer as a
parameter*. This is the honest-signature rule applied to output: a function
that prints has a `Writer` in its signature, and a function without one
cannot print.

```k2
const std = @import("std");

/// This function can write. Its `Writer` parameter is the visible proof.
/// It cannot reach stdout on its own — only the writer it was handed.
fn greet(w: anytype, name: []const u8) !void {
    try w.print("Hello, {s}!\n", .{name});
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try greet(out, "k2");
}
```

`w: anytype` is the idiomatic way to accept "any writer": it is a comptime
parameter (§07) monomorphized per concrete writer type, so a known writer
devirtualizes to a direct call — the zero-cost-abstraction guarantee. You
may instead spell it `w: Writer` to take the interface by value when you
want one shared code path.

### 4.3 The `Reader` interface

A `Reader` is the dual: an interface over anything that can supply bytes. Its
primitive is `read` (fill a caller-provided buffer, returning how many bytes
were read); helpers like `readByte` and `readUntilDelimiter` build on it.

```k2
const std = @import("std");

/// Sum the bytes of one line read from `r` into a CALLER-PROVIDED buffer.
/// No allocation: the buffer is passed in, never grabbed from a hidden heap.
fn checksumLine(r: anytype, buf: []u8) !u32 {
    // `readUntilDelimiter` fills `buf`; it does not allocate.
    const line = try r.readUntilDelimiter(buf, '\n');
    var sum: u32 = 0;
    for (line) |b| sum +%= b;
    return sum;
}

pub fn main(sys: *System) !void {
    const in = sys.io.stdin();   // a Reader over standard input
    const out = sys.io.stdout();

    var line_buf: [256]u8 = undefined; // caller owns the buffer
    const sum = try checksumLine(in, &line_buf);
    try out.print("checksum = {d}\n", .{sum});
}
```

Note the pattern that recurs everywhere in `std.io`: **the caller provides
the buffer.** `read`, `readUntilDelimiter`, and friends fill memory you
already own. They never allocate. When you want a reader that *grows* a
buffer for you, you reach for `r.readAllAlloc(alloc, max)` — and the moment a
reader routine can allocate, it takes an `Allocator` in its signature, just
like everything else.

### 4.4 Writers over memory: no I/O at all

Because `Writer` is an interface, you can construct one over a plain byte
buffer with no I/O capability whatsoever. This is how you format into memory:
build a `FixedBufferWriter` over stack storage and `print` into it.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    var storage: [64]u8 = undefined;
    // A writer over a fixed buffer — pure memory, no Io capability needed.
    var fbw = std.io.FixedBufferWriter.init(&storage);
    const w = fbw.writer();

    // Formats into `storage`; allocates nothing, performs no I/O.
    try w.print("answer = {d}", .{42});

    // Only NOW, with the io capability, do we actually emit bytes.
    const out = sys.io.stdout();
    try out.print("[{s}]\n", .{fbw.written()});
}
```

A `FixedBufferWriter` needs neither an `Allocator` nor an `Io`: it writes
into a buffer you already hold. This is the in-memory analogue of §03's
fixed-buffer allocator, and it is the seam where formatting (pure) meets I/O
(a capability): you format into memory with no authority, then spend exactly
one capability to flush.

---

## 5. Formatting: `std.fmt`

`std.fmt` turns values into text. It is a **pure** module: it neither
allocates nor performs I/O. It only ever **writes into a `Writer` or a
buffer you provide.** This is what lets `out.print` work without an
allocator and what lets you format into a fixed stack buffer with zero
authority.

### 5.1 `format` — the engine behind `print`

The core routine is `std.fmt.format(writer, comptime fmt, args)`. It parses
the comptime-known format string, and for each placeholder it formats the
corresponding argument straight into `writer`. `Writer.print` is just a thin
forwarder to it (see §4.1).

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // These are equivalent; `print` forwards to `format`.
    try std.fmt.format(out, "x = {d}\n", .{7});
    try out.print("x = {d}\n", .{7});
}
```

Because `fmt` is **comptime-known**, the placeholder/argument match is
checked at compile time: a wrong arity or an unformattable type is a
`@compileError`, not a runtime surprise. No allocation occurs at any point;
each argument is rendered directly into the writer's sink.

### 5.2 Formatting into a buffer with `bufPrint`

When you want text in memory rather than emitted, `std.fmt.bufPrint` formats
into a **caller-provided buffer** and returns the written subslice. It takes
no allocator and no capability — just your buffer.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    var buf: [32]u8 = undefined;
    // Formats into `buf`; returns the used prefix. No allocation, no I/O.
    const text = try std.fmt.bufPrint(&buf, "{d}-{s}", .{ 2, "k2" });

    const out = sys.io.stdout();
    try out.print("rendered: {s} (len {d})\n", .{ text, text.len });
}
```

`bufPrint` returns `error{NoSpaceLeft}![]u8`: it fails if your buffer is too
small, which is a value you handle, never a hidden reallocation. If you
*want* `std.fmt` to allocate the exact-sized result for you, you call
`std.fmt.allocPrint(alloc, fmt, args)` — and, predictably, the instant
formatting allocates, it takes an `Allocator` and transfers ownership of the
result to you:

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const alloc = sys.heap;

    // The ONE allocating formatter: it takes an allocator and you free it.
    const msg = try std.fmt.allocPrint(alloc, "id={d}", .{99});
    defer alloc.free(msg);

    const out = sys.io.stdout();
    try out.print("{s}\n", .{msg});
}
```

The trio — `format` (into a writer), `bufPrint` (into your buffer),
`allocPrint` (into a fresh allocation) — is `std.fmt`'s entire output
surface, and the rule is consistent: only `allocPrint` takes an allocator,
because only `allocPrint` allocates.

### 5.3 The format-string mini-language (high level)

A format string is plain text with `{...}` placeholders. Each placeholder is
`{[specifier]}`, where the specifier selects how the matching argument is
rendered. The set is small and fixed (One obvious way, small surface):

| Specifier | Renders | Example argument → output |
| --- | --- | --- |
| `{d}` | Decimal integer | `42` → `42` |
| `{x}` / `{X}` | Lower/upper hex | `255` → `ff` / `FF` |
| `{b}` / `{o}` | Binary / octal | `5` → `101` / `5` |
| `{c}` | Single byte as a character | `65` → `A` |
| `{s}` | UTF-8 string (`[]const u8`) | `"k2"` → `k2` |
| `{e}` / `{d}` (float) | Scientific / decimal float | `1.5` → `1.5e0` / `1.5` |
| `{any}` | Default representation of any type (uses reflection) | `Point{...}` → `Point{ .x = 1, .y = 2 }` |
| `{{` / `}}` | A literal `{` / `}` | — |

Width, alignment, and precision attach to the specifier
(e.g. `{d:0>4}` zero-pads to width 4, `{d:.2}` for two decimals), following
the same compact grammar. Crucially:

- The format string is **comptime-known**, so the parse and the
  argument-count check happen at compile time.
- `{any}` uses `@typeInfo` reflection to render aggregates field-by-field —
  ordinary comptime k2 (§07), no runtime type information, no allocation.
- A type may define a `pub fn format(self, writer, ...) !void` method to
  customize how `{any}` (or a custom specifier) renders it; `std.fmt` calls
  that method, passing the *writer* — so even custom formatting cannot
  allocate or perform I/O unless it was handed the capability to.

The takeaway: formatting is a pure transformation from values to bytes that
*lands somewhere you chose*. It is never a side effect on its own.

---

## 6. Pure computational modules

These modules compute and nothing else. They take **no capability** — no
allocator, no I/O, no clock — because they need none, and their signatures
say so. They are the bulk of `std` by surface area and the part you can call
from anywhere, including comptime (§07), without threading any authority.

### 6.1 `std.math`

Numeric routines and constants over the primitive types. Pure functions:
they read their arguments and return a value.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const h = std.math.sqrt(@as(f64, 3.0 * 3.0 + 4.0 * 4.0)); // 5.0
    const m = std.math.max(@as(i32, -7), 3);                  // 3
    const clamped = std.math.clamp(@as(i32, 12), 0, 10);      // 10

    const out = sys.io.stdout();
    try out.print("h={d} m={d} clamped={d}\n", .{ h, m, clamped });
}
```

Coverage includes `min`/`max`/`clamp`, `abs`, `pow`/`sqrt`/`log`/`exp`, the
trig family, `gcd`/`lcm`, the constants `pi`/`e`, and the
checked/saturating/wrapping arithmetic helpers (`add`, `sub`, `mul` that
return `error{Overflow}!T` for explicit overflow handling rather than the
safe-build panic). All are pure; none allocate.

### 6.2 `std.sort`

In-place and comparison-based sorting over slices you already own. `std.sort`
**sorts the slice in place** and so allocates nothing; you pass a comparator
as a comptime function.

```k2
const std = @import("std");

fn lessThan(_: void, a: i32, b: i32) bool {
    return a < b;
}

pub fn main(sys: *System) !void {
    var xs = [_]i32{ 5, 2, 9, 1, 7 };
    // Sorts `xs` in place; no allocation, the slice IS the storage.
    std.sort.sort(i32, &xs, {}, lessThan);

    const out = sys.io.stdout();
    try out.print("sorted: {any}\n", .{xs});
}
```

`std.sort` also provides `binarySearch`, `isSorted`, and a stable variant.
None take an allocator: they operate on the slice in place. If a sort needed
scratch space, it would take an `Allocator` — but the default introsort
sorts in place, so it does not.

### 6.3 `std.hash`

Non-cryptographic hashing of byte slices: `Wyhash`, `Fnv1a`, `CRC32`, and the
generic `autoHash` that hashes any value by reflecting over its fields. Pure:
it consumes bytes and produces an integer.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const h = std.hash.Wyhash.hash(0, "k2"); // seed 0, returns a u64

    const out = sys.io.stdout();
    try out.print("hash = {x}\n", .{h});
}
```

This is the machinery `AutoHashMap` (§3.2) uses to derive a key's hash — the
map calls `std.hash.autoHash` over the key type at comptime. Note that
`std.hash` is for *hash tables and checksums*; cryptographic hashing and the
CSPRNG live behind the `Random` capability (§10) precisely because secure
randomness is authority over the outside world, while a hash function is pure
math.

### 6.4 `std.ascii` and `std.unicode`

Character classification and case operations. `std.ascii` works on single
bytes (`isDigit`, `isAlpha`, `toLower`, `toUpper`); `std.unicode` handles
UTF-8 (decoding code points, validating, iterating). Both are pure — they
classify and transform bytes/code points and allocate nothing.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // ASCII classification: pure byte predicates.
    try out.print("'7' digit? {}\n", .{std.ascii.isDigit('7')});

    // UTF-8: count code points in a string, no allocation.
    const n = try std.unicode.utf8CountCodepoints("café");
    try out.print("code points: {d}\n", .{n});
}
```

Routines that *transform* into a new buffer (e.g. ASCII-lowercasing a whole
string) take a caller-provided destination buffer or, if they must size the
result, an `Allocator` — the same split as `std.mem` (§2.3 vs §2.4).

### 6.5 `std.testing`

`std.testing` is the assertion and harness module used inside `test` blocks
(`test` is a charter keyword). Its assertions are pure comparisons that
return an error union, so a failed assertion is an ordinary error value the
test runner reports.

```k2
const std = @import("std");

fn doubled(x: u32) u32 {
    return x * 2;
}

test "doubled multiplies by two" {
    try std.testing.expectEqual(@as(u32, 84), doubled(42));
    try std.testing.expect(doubled(0) == 0);
    // expectError asserts a specific error is returned.
    // try std.testing.expectError(error.Empty, parse(""));
}
```

The key members:

| Member | Purpose |
| --- | --- |
| `expect(cond)` | Fail unless `cond` is true. |
| `expectEqual(expected, actual)` | Fail unless equal (`{any}`-diffs on mismatch). |
| `expectError(err, expr)` | Fail unless `expr` returns exactly `err`. |
| `expectEqualSlices(T, a, b)` | Element-wise slice equality. |
| `allocator` | A **leak-detecting** `Allocator` the harness hands each test. |

`std.testing.allocator` is the capability that makes the "no ambient
authority" promise pay off in tests: because every container and every
allocating function takes its allocator explicitly, a test simply passes the
leak-detecting one, and any forgotten `free`/`deinit` fails the test
deterministically.

```k2
const std = @import("std");

test "ArrayList is leak-clean under the testing allocator" {
    var list = std.ArrayList(u32).init(std.testing.allocator);
    defer list.deinit(); // remove this and the test FAILS with a leak report

    try list.append(1);
    try list.append(2);
    try std.testing.expectEqual(@as(usize, 2), list.items.len);
}
```

---

## 7. The capability types from `*System`

This is the heart of **No ambient authority**. Every power over the world
outside pure computation is a capability *value*, reachable only through the
single root handle `*System` that `main` receives:

```k2
pub fn main(sys: *System) !void { ... }
```

From `sys` you obtain *narrower* capabilities, each granting exactly one kind
of authority. There are **no global functions that reach the OS** — no
`std.print`, no `time.now()`, no `os.getenv()` that works without a value.

### 7.1 The seven capabilities

| Capability | Obtained as | Grants — and *only* — |
| --- | --- | --- |
| `Allocator` | `sys.heap` | Heap memory: `alloc`/`free`/`realloc`/`create`/`destroy` (§02, §05). |
| `Io` | `sys.io` | Standard streams (`stdout`/`stderr`/`stdin`) and the file-opening entry point (§04). |
| `Clock` | `sys.clock` | The monotonic and wall clocks: reading time, sleeping. |
| `Random` | `sys.random` | A CSPRNG handle: secure and seedable random bytes/integers. |
| `Fs` | `sys.fs` | The filesystem: opening, reading, writing, listing paths. |
| `Net` | `sys.net` | The network: connecting, listening, sockets. |
| `Env` | `sys.env` | Process environment variables and command-line arguments. |

Each is a plain struct of function pointers plus context (like `Allocator`),
so it costs nothing beyond an indirect call and is devirtualized when the
concrete type is known. The signature of a capability is its complete
contract: a `Clock` can read time and do *nothing else* — it cannot allocate,
cannot print, cannot open a socket.

### 7.2 `Clock` — reading time

```k2
const std = @import("std");

/// This function can read the clock. Its `Clock` parameter is the proof.
/// It CANNOT allocate, print, or touch the network — only read time.
fn elapsedSince(clock: Clock, start_ns: u64) u64 {
    return clock.monotonicNanos() - start_ns;
}

pub fn main(sys: *System) !void {
    const start = sys.clock.monotonicNanos();
    // ... do work ...
    const dt = elapsedSince(sys.clock, start);

    const out = sys.io.stdout();
    try out.print("elapsed: {d} ns\n", .{dt});
}
```

`Clock` exposes `monotonicNanos()` (a steadily increasing counter for
measuring durations), `wallNanos()` (calendar time), and `sleep(ns)`. A
function that takes a `Clock` and nothing else is, by signature, a function
whose only effect is observing or waiting on time.

**Deterministic vs. real time (v0.23).** `sys.clock` is the *deterministic*
clock: it starts at zero and only advances on `sleep`, so it is fully
reproducible — the default for tests. Alongside it, `sys.time` reads the
**real host clock**: `sys.time.monotonicReal()` (a non-decreasing real
monotonic counter, nanoseconds), `sys.time.nowReal()` (the real wall-clock
Unix time, nanoseconds), and `sys.time.sleepReal(ns)` (a real delay). The
`std.time.Duration`/`std.time.Instant` value types work over either clock —
`Instant.fromNanos(reading)` captures a point and `elapsedSince(later_reading)`
measures a `Duration`. Real time is opt-in *per call*, so adding `sys.time`
never perturbs a deterministic run. Tests of real time assert only
inequalities (monotonic increased, a sleep delayed by at least a loose lower
bound), never exact nanoseconds, so they stay robust and offline.

### 7.3 `Random` — randomness

```k2
const std = @import("std");

/// Roll an n-sided die. Needs randomness, so it takes a `Random`. It cannot
/// do anything but draw random values.
fn rollDie(rng: Random, sides: u32) u32 {
    return rng.intRangeLessThan(u32, 0, sides) + 1;
}

pub fn main(sys: *System) !void {
    const roll = rollDie(sys.random, 6);

    const out = sys.io.stdout();
    try out.print("you rolled a {d}\n", .{roll});
}
```

`Random` exposes `int(T)`, `intRangeLessThan(T, lo, hi)`, `float(T)`, and
`bytes(buf)`. There is **no global rng**; randomness flows from `sys.random`
just as the heap flows from `sys.heap`. This is what makes random-using code
testable: hand it a *seeded* or *scripted* `Random` (§08) and its behavior is
deterministic.

### 7.4 `Fs`, `Net`, `Os`, and `Env` (v0.23)

The remaining capabilities follow the identical pattern: authority is a
value, obtained from `sys`, and a function that uses it carries it in its
signature. As of v0.23 these are **real OS effects** (backed by the host's
filesystem and TCP stack); tests use temp files and loopback only, are
self-cleaning, and never touch an external network.

**`Fs` — the filesystem.** `sys.fs` opens, reads, writes, stats, and removes
files, and makes/removes directories:

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = "/tmp/k2_demo.bin";

    // Create + write through the fs capability (no ambient `open`).
    var wf = try sys.fs.create(path);          // O_CREAT|O_TRUNC|O_RDWR
    const nw = try wf.write("hello, fs");       // -> bytes written
    wf.close();

    // Stat: size and is_dir.
    const st = try sys.fs.stat(path);
    try out.print("wrote {d}, size {d}\n", .{ nw, st.size });

    // Read it back, byte-for-byte.
    var rf = try sys.fs.openRead(path);         // read-only
    var buf: [64]u8 = undefined;
    const nr = try rf.read(&buf);
    rf.close();
    try out.print("read {d}: {s}\n", .{ nr, buf[0..nr] });

    try sys.fs.delete(path);                     // self-clean
}
```

`Fs` exposes `openRead(path)`, `create(path)`, `openReadWrite(path)` (each →
`FsError!File`), `stat(path)`, `exists(path)`, `delete(path)`,
`makeDir(path)`/`removeDir(path)`, and `listDir(alloc, path)`. A `File`
exposes `read(buf)`/`write(bytes)`/`stat()`/`close()`. Errors are an honest
`FsError` set (`FileNotFound`, `AccessDenied`, `AlreadyExists`, …) mapped from
the host.

**`Net` — TCP sockets.** `sys.net.listen(port)` binds a loopback listener
(port 0 = an ephemeral port, read back with `localPort()`); `sys.net.connect(
host, port)` connects. A `TcpListener.accept()` yields a `TcpStream`, which
exposes `send(bytes)`/`recv(buf)`/`close()`:

```k2
var listener = try sys.net.listen(0);          // ephemeral loopback port
var client   = try sys.net.connect("127.0.0.1", listener.localPort());
var server   = try listener.accept();
_ = try client.send("ping");
var buf: [16]u8 = undefined;
const n = try server.recv(&buf);               // server reads "ping"
_ = try server.send(buf[0..n]);                // echo it back
```

**`Os` and `Env` — process.** `sys.os.argCount()`/`sys.os.arg(i)` read the
forwarded command-line arguments (everything after `--`); `sys.os.args(alloc)`
materializes them as `[][]const u8` through an explicit allocator (so the
allocation is visible). `sys.os.getpid()` and `sys.os.exit(code)` round out the
process surface. `sys.env.get(name)` looks up an environment variable, returning
`?[]const u8`. **Reproducibility default:** env lookups are *offline-absent* by
default — `sys.env.get` consults the host environment only when the run opts in
(`k2c run --real-env`), or a scripted value supplied with `--env=KEY=VALUE`;
otherwise every lookup is `null`. Likewise `getpid()` is a deterministic `1`
unless `--real-pid` is passed. This keeps every run reproducible by default;
the idiomatic form is `sys.env.get("VAR") orelse "default"`.

**Native backend (v0.23).** The VM backs every effect with Rust `std`. The
native backend implements the feasible subset with raw Linux syscalls
(`sys.os.getpid`/`exit`) and **cleanly refuses** the rest at compile time
(`sys.fs`/`sys.net`/`sys.time`, `os.args`/`env.get`), so such a program runs on
the VM (`k2c run`) — a refusal is always reported, never a miscompile.

### 7.5 The rule, stated precisely

> **Code can only do what it was handed the capability to do.** A function's
> authority is the union of the capabilities in its signature, transitively.
> If `*System` is never threaded to a function, that function cannot reach
> the OS at all; if only an `Allocator` is threaded, it can allocate but not
> print, time, or connect. There is no escape hatch, no ambient namespace,
> no `std` member that quietly reaches past its parameters. The capability
> list *is* the function's contract with the world.

This is k2's defining extension of Zig's "no hidden allocation" to *all*
power, and it is why the standard library has the exact shape laid out in
§1–§6: every facility is sorted by the authority it requires, and that
authority is always a value you can see.

---

## 8. Testing with mock capabilities

Because every effect is a passed value, you test effectful code by passing
**fake** capabilities and asserting on the result — no real clock, no real
randomness, no real network, no real time elapsed. This is not a bolt-on
testing framework; it is the direct, mechanical consequence of "no ambient
authority." If the code under test reached for an ambient global, you could
not substitute it; because it takes a capability, you simply hand it another.

### 8.1 A fake clock makes time deterministic

```k2
const std = @import("std");

/// Logic under test: time a unit of work. It takes a `Clock` capability, so
/// in a test we hand it a FAKE clock and the result is deterministic.
fn measure(clock: anytype) u64 {
    const start = clock.monotonicNanos();
    clock.advanceForWork(); // (the fake advances; a real clock would not)
    return clock.monotonicNanos() - start;
}

test "measure reports the scripted elapsed time" {
    // A fake clock that ticks by a fixed amount on each advance — no real
    // wall-clock dependency, no flakiness, fully deterministic.
    var clock = std.testing.FakeClock.init(.{ .step_ns = 100 });
    try std.testing.expectEqual(@as(u64, 100), measure(&clock));
}
```

### 8.2 A seeded RNG makes randomness reproducible

```k2
const std = @import("std");

fn rollDie(rng: anytype, sides: u32) u32 {
    return rng.intRangeLessThan(u32, 0, sides) + 1;
}

test "rollDie is reproducible under a seeded rng" {
    // Same seed → same sequence. The capability model is what lets us pin it.
    var rng = std.testing.SeededRandom.init(0xC0FFEE);
    const a = rollDie(&rng, 6);

    var rng2 = std.testing.SeededRandom.init(0xC0FFEE);
    const b = rollDie(&rng2, 6);

    try std.testing.expectEqual(a, b); // identical, deterministically
}
```

### 8.3 A mock writer captures output without I/O

```k2
const std = @import("std");

/// Produces output through a writer it was handed. In a test we hand it an
/// in-memory writer and assert on the captured bytes — no terminal involved.
fn renderGreeting(w: anytype, name: []const u8) !void {
    try w.print("Hello, {s}!", .{name});
}

test "renderGreeting writes the expected bytes" {
    var storage: [64]u8 = undefined;
    var sink = std.io.FixedBufferWriter.init(&storage);

    try renderGreeting(sink.writer(), "k2");

    // The output landed in memory; assert on it directly.
    try std.testing.expectEqualSlices(u8, "Hello, k2!", sink.written());
}
```

### 8.4 Sandboxing is the same mechanism

Testing and sandboxing are the *same* operation viewed from two angles: in a
test you pass a fake capability to *observe* behavior; in a sandbox you pass a
*restricted* capability to *limit* behavior. Hand a program a `Net` that
refuses all connections, an `Fs` rooted at a temp directory, and a `Clock`
frozen at a fixed instant, and the program physically cannot exceed those
bounds — not by policy, but because those values are the only authority it
holds.

```k2
const std = @import("std");

/// Run untrusted logic with a System whose capabilities are all mocked or
/// restricted. The logic cannot exceed what these fakes permit, because they
/// are the ONLY authority it has.
fn runSandboxed(sandbox: *System) !void {
    // Inside here, `sandbox.fs` might be chrooted, `sandbox.net` might deny
    // all, `sandbox.clock` might be frozen — the code cannot tell or escape.
    const out = sandbox.io.stdout();
    try out.print("running inside the sandbox\n", .{});
}

test "sandboxed code only sees the capabilities it was given" {
    var fake = std.testing.MockSystem.init(.{
        .deny_net = true,
        .frozen_clock_ns = 0,
    });
    try runSandboxed(fake.system());
    // Assert the code performed no forbidden effect:
    try std.testing.expect(fake.net_connect_count == 0);
}
```

This is the testing/sandboxing payoff stated in the charter: code is
"trivially sandboxable (hand a mock `System`), deterministically testable
(hand a fake clock and seeded RNG), and auditable (effects are visible at
every call site)." None of it requires special tooling — it falls out of
representing authority as ordinary values.

---

## 9. Module index

A map of the standard library by authority category. The "Needs" column is
the load-bearing one: it is exactly what the module's functions take, and
therefore exactly what they can do.

| Module / type | Category | Needs | What it provides |
| --- | --- | --- | --- |
| `std.mem` (views) | Pure | nothing | `copy`, `set`, `eql`, `indexOf`, `split`, `trim` — operate in place / return views. |
| `std.mem` (`dupe`, `join`) | Allocating | `Allocator` | Build new heap byte runs; transfer ownership. |
| `std.math` | Pure | nothing | Numeric functions, constants, checked arithmetic. |
| `std.sort` | Pure | nothing | In-place sort, `binarySearch`, `isSorted`. |
| `std.hash` | Pure | nothing | `Wyhash`, `Fnv1a`, `CRC32`, `autoHash`. |
| `std.ascii` / `std.unicode` | Pure | nothing | Classification, case, UTF-8 decode/validate. |
| `std.fmt` | Pure | nothing (or `Allocator` for `allocPrint`) | `format`/`bufPrint` into a writer/buffer; `allocPrint` allocates. |
| `std.io` | Interfaces | nothing to define; `Io` to get a real stream | `Writer`/`Reader` shapes; `FixedBufferWriter`. |
| `std.testing` | Test harness | the testing allocator/mocks it hands you | `expect*`, leak-detecting `allocator`, fakes. |
| `std.ArrayList(T)` | Container | `Allocator` | Growable sequence; `init`/`deinit`. |
| `std.HashMap` / `AutoHashMap` | Container | `Allocator` | Unordered map; `init`/`deinit`. |
| `std.HashSet(T)` | Container | `Allocator` | Membership set; `init`/`deinit`. |
| `std.ArrayHashMap` | Container | `Allocator` | Insertion-ordered map; `init`/`deinit`. |
| `std.BoundedArray(T, N)` | Container | **nothing** | Fixed-capacity inline array; no allocator, no `deinit`. |
| `std.heap.*` | Allocators | a backing source / `sys` | Page, arena, fixed-buffer, GPA (see §05). |
| `Allocator` | Capability | from `sys.heap` | Heap memory. |
| `Io` | Capability | from `sys.io` | Standard streams + file opening. |
| `Clock` | Capability | from `sys.clock` | Monotonic/wall time, sleep. |
| `Random` | Capability | from `sys.random` | CSPRNG. |
| `Fs` | Capability | from `sys.fs` | Filesystem. |
| `Net` | Capability | from `sys.net` | Network. |
| `Env` | Capability | from `sys.env` | Environment + args. |

Read the table top to bottom and the library's design principle is visible
as a gradient: pure at the top (needs nothing), allocating in the middle
(needs an `Allocator`), full authority at the bottom (needs a capability from
`*System`). A facility sits exactly as high as the authority it requires,
and never higher.

---

## 10. Summary

- `std` is one module with **three categories**: **pure** modules that take
  no capability (`math`, `sort`, `hash`, `ascii`/`unicode`, `fmt`, the
  `mem` view routines); **allocator-taking** facilities (every container,
  `mem.dupe`/`join`, `fmt.allocPrint`); and **capability types** obtained
  from `*System`.
- **`std.mem`** hosts the `Allocator` interface (referenced from §05, never
  redefined) and pure byte/slice routines; the ones that *view* take
  nothing, the ones that *build* take an `Allocator`.
- **Containers** — `ArrayList`, `HashMap`/`AutoHashMap`, `HashSet`,
  `ArrayHashMap` — every constructor takes an `Allocator`, stores it, and
  frees with it via `deinit`, paired with `defer` at creation.
  `BoundedArray(T, N)` is the heap-free exception: inline storage, no
  allocator, no `deinit`.
- **`std.io`** defines `Writer`/`Reader` *interfaces* but grants **no global
  stdout/stderr/stdin**. Real streams come only from `sys.io`. Formatted
  printing goes through an explicit writer and allocates nothing.
- **`std.fmt`** formats into a writer (`format`), a buffer (`bufPrint`), or —
  only when you ask it to allocate — a fresh allocation (`allocPrint`,
  which takes an `Allocator`). It performs no I/O and no hidden allocation;
  the format string is comptime-checked.
- The **capability types** `Allocator`, `Io`, `Clock`, `Random`, `Fs`,
  `Net`, `Env` flow from `sys.heap`/`sys.io`/`sys.clock`/… Each grants
  exactly one kind of authority, and **code can only do what it was handed
  the capability to do.**
- **Mock capabilities** make effectful code trivially testable and
  sandboxable: pass a fake clock, a seeded RNG, an in-memory writer, or a
  restricted `System`, and the code's behavior is deterministic and bounded
  — because those values are its only authority.

This is the standard library in one sentence: **every module is sorted by
the authority it needs, that authority is always an explicit value, and so a
function's signature is a complete, honest account of what it can allocate
and what it can do to the world.**

---

### See also

- **§05 — Memory and allocators:** the `Allocator` interface this chapter
  references — its `alloc`/`free`/`realloc`/`create`/`destroy` contract, the
  `std.heap` strategies, and the ownership/`defer`/`errdefer` conventions
  every container here obeys.
- **§04 — Functions:** the `anytype` comptime parameter used for `Writer`/
  `Reader` and the honest-signature rule that a function's authority is its
  parameter list.
- **§06 — Error handling:** `try`/`catch` on the error unions every
  allocating and I/O routine returns (`OutOfMemory`, `NoSpaceLeft`, write
  errors), and why those are values, not exceptions.
- **§07 — comptime:** generics-as-functions (`ArrayList(T)`,
  `AutoHashMap(K, V)`), the comptime-known format string, and the
  `@typeInfo` reflection behind `{any}` and `autoHash`.
- **§08 — Modules and the build system:** `@import("std")`, the
  `pub fn main(sys: *System) !void` entry point that delivers the root
  capability, and `sys.env.args` for command-line arguments.
- **§09 — Concurrency:** the `Executor` and `std.event.Loop` capabilities
  built from `sys.heap`, and the same mock-capability testing technique
  (`std.testing.FakeLoop`) applied to concurrent code.
