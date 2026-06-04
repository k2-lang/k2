# 09 — Concurrency

> Part of the **k2** language specification.
> *k2: total control over the machine, with zero waste.*

This chapter specifies how concurrent and asynchronous code is written in k2:
how OS threads are obtained as explicit capabilities, how shared state is
guarded with atomics and synchronization primitives from `std`, how the
`*T`/`*const T` discipline replaces a borrow checker, how structured
concurrency is expressed with `defer`, and how k2's colorless, stackless
async model lowers to a state machine driven by an *explicit* event-loop
capability — with no hidden runtime, no scheduler, and no green threads linked
into your binary.

> **k2 has no `async`/`await` keywords.** The locked 35-keyword set
> ([§1 of the lexical chapter](01-lexical-structure.md#5-keywords)) does not
> include `async`, `await`, `suspend`, or `resume`, and the grammar has no
> async/await production. k2's async model is therefore expressed *entirely as
> library calls on an event-loop capability*: an async call is a method that
> returns a `Frame` value, and a frame is driven to completion by calling a
> method on it. Suspension happens **inside ordinary function calls**, so it
> stays inside the closed control-flow set
> ([§11 of the expressions chapter](03-expressions-and-statements.md#11-the-closed-set-restated))
> and adds no new surface syntax. The precise method set of that capability is
> still stabilizing toward v1 (§10).

The governing principle of this chapter is that **parallelism and asynchrony
are powerful effects, so they are made visible and passed as values like any
other authority.** Two charter pillars apply with full force:

> **No hidden control flow.** The only non-linear control flow is the explicit
> set: `if`/`else`, `while`, `for`, `switch`, `try`, `catch`, `defer`,
> `errdefer`, `orelse`, `return`, `break`, `continue`, and `unreachable`.

> **No ambient authority.** I/O, the clock, randomness, the environment, and
> the filesystem are capability values that must be passed explicitly from
> `main` downward.

Threads, executors, and event loops are *exactly* this kind of authority. None
of them is ambient; all of them are built from the capabilities that flow from
`*System` — concretely, from `sys.heap`, the allocator capability that every
concurrency object stores and grows through.

---

## Table of contents

1. [Overview and stance](#1-overview-and-stance)
2. [Threads via an explicit `Executor` capability](#2-threads-via-an-explicit-executor-capability)
3. [The `*T` / `*const T` data-race discipline](#3-the-t--const-t-data-race-discipline)
4. [Atomics and memory ordering](#4-atomics-and-memory-ordering)
5. [Synchronization primitives](#5-synchronization-primitives)
6. [Structured concurrency](#6-structured-concurrency)
7. [Async: colorless, stackless, no runtime, no keywords](#7-async-colorless-stackless-no-runtime-no-keywords)
8. [The event loop is an explicit capability](#8-the-event-loop-is-an-explicit-capability)
9. [Testing concurrent code](#9-testing-concurrent-code)
10. [v1 vs future](#10-v1-vs-future)
11. [Summary](#11-summary)

---

## 1. Overview and stance

k2 has **no built-in concurrency runtime**. There is no green-thread
scheduler, no work-stealing executor baked into the language, no
garbage-collected async machinery, and no language runtime linked into your
binary. This is the same `no runtime` commitment that governs memory: the
language provides primitives and a capability model; the *strategy* lives in
`std` and is chosen by you at the point where you create it.

Three facts define the whole stance:

1. **Concurrency is library-provided over OS threads.** A thread is an OS
   resource. You obtain the *capability* to spawn one by **constructing an
   `Executor`** — a `std.Thread.Pool` — over the heap capability you were
   handed, and you pass that `Executor` down to the code that needs to spawn
   work. There is no global `Thread.spawn` that works without a capability,
   just as there is no global `print` or `time.now()`. This matches the
   standard-library chapter, where `std.Thread` and `std.atomic` touch the
   world "only via a passed `Executor`."
2. **Shared state is guarded explicitly.** There is no implicit locking, no
   hidden synchronization, no automatic memory barrier. You reach for
   `std.atomic`, `std.Thread.Mutex`, and friends, and every barrier is written
   where it happens.
3. **Async is a compile-time transformation, not a runtime, and uses no new
   keywords.** An async call is an *ordinary method call* on an event-loop
   capability that returns a `Frame` value whose storage the *caller* owns; a
   second method call drives the frame to completion. The event loop that
   drives frames is itself a value you construct from `sys.heap` and pass
   explicitly. The same function can run blocking or evented depending on the
   capability it is handed.

Because every one of these is a capability or a plain value, a function's
signature is an honest account of its concurrent reach. A function that takes
only an `Allocator` cannot spawn a thread, cannot drive an event loop, and
cannot reach the network — the same audit-by-signature that holds everywhere
else in k2. (An `Executor` or an event `Loop` is *built from* an `Allocator`,
but it is a distinct value with its own type: holding an `Allocator` does not
by itself let you spawn — you must be handed, or construct, the executor.)

> **Data-race freedom is a discipline, not a borrow checker.** k2 deliberately
> ships no borrow checker (it would violate the small-surface pillar). Instead,
> race freedom is enforced by the `*T` (exclusive) versus `*const T` (shared)
> convention, by the safety allocator's tooling in Debug/ReleaseSafe, and by
> your explicit use of synchronization primitives. See §3.

> **Why no `sys.threads` field?** The root `*System` exposes exactly six
> capabilities — `heap`, `io`, `clock`, `random`, `env`, `net`
> ([standard-library chapter §10.0](10-standard-library.md#100-the-root-system)) —
> and nothing reaches the OS outside them. Concurrency does **not** add a
> seventh field: an `Executor` and an event `Loop` are *library objects* you
> build from `sys.heap`, exactly as `ArrayList` and `HashMap` are. The OS
> threads an `Executor` manages are its private business, requested when you
> construct it from the heap capability — visibly, at the call site.

---

## 2. Threads via an explicit `Executor` capability

### 2.1 Obtaining the executor capability

You do not call a global to start a thread. You **construct an `Executor`** —
the spawning capability — from the heap capability `sys.heap`, then pass that
`Executor` to the code that needs to spawn work. The `Executor` owns its OS
threads; creating it is the visible point where OS-thread authority enters
your program.

```k2
const std = @import("std");

/// `std.Thread.Executor` is the thread-spawning capability. It is built from
/// the heap capability and owns the OS threads it spawns. Nothing below spawns
/// a thread without an `Executor` in hand.
pub fn main(sys: *System) !void {
    // Construct the executor over the heap capability. `init` is where OS
    // threading authority becomes available — visibly, via a constructor.
    var exec = try std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    // The executor owns OS threads; `deinit` joins them. No implicit destructor.
    defer exec.deinit();

    const out = sys.io.stdout();

    // Spawn one task running `worker`, passing it an argument tuple.
    // `try` propagates the OS error if a thread cannot be created.
    const handle = try exec.spawn(worker, .{ out, @as(u32, 7) });

    // A task handle is a resource. Pair it with its release in the same scope:
    // `join` blocks until the task finishes. There are no implicit destructors,
    // so the join is written explicitly.
    defer handle.join();

    try out.print("main: spawned worker\n", .{});
}

fn worker(out: anytype, n: u32) void {
    // Runs on an executor thread. It received `out` and `n` by value — it has
    // no ambient authority of its own beyond what was handed to it.
    out.print("worker: n = {d}\n", .{n}) catch {};
}
```

Key properties:

- **`init` and `spawn` return error unions.** Building the executor and
  creating an OS thread can fail (`error.ThreadQuotaExceeded`,
  `error.OutOfMemory`, …). That failure is an ordinary value handled with
  `try`/`catch`, never an exception.
- **A task handle is a resource.** It is closed with `join` (wait for
  completion) or `detach` (give up the right to wait). One of the two must
  happen exactly once; the idiom is `defer handle.join()` on the spawning
  scope. In Debug/ReleaseSafe, dropping a handle without joining or detaching
  is flagged the same way a leaked allocation is.
- **No ambient authority crosses the boundary.** The worker function receives
  only the values passed in the argument tuple. If `worker` needs the heap, it
  must be handed an `Allocator`; if it needs the clock, a clock capability. A
  thread is not a backdoor around the capability model.

### 2.2 Thread arguments and the stack/heap question

The argument tuple passed to `spawn` is *copied* into the task's frame; no
heap allocation happens beyond the executor's own bookkeeping (which it does
through the allocator you handed `init`). If you want the worker to share heap
state, you pass a pointer — and now §3 and §4 govern how that sharing is made
safe.

```k2
const std = @import("std");

const Counter = struct {
    // An atomic integer: see §4. Shared across threads, mutated without locks.
    value: std.atomic.Value(u64),
};

fn bump(counter: *Counter, times: u64) void {
    var i: u64 = 0;
    while (i < times) : (i += 1) {
        // Read-modify-write as one atomic step, monotonic ordering suffices
        // for a pure tally.
        _ = counter.value.fetchAdd(1, .monotonic);
    }
}

pub fn main(sys: *System) !void {
    var exec = try std.Thread.Executor.init(sys.heap, .{ .worker_count = 2 });
    defer exec.deinit();
    const out = sys.io.stdout();

    var counter = Counter{ .value = std.atomic.Value(u64).init(0) };

    // Both tasks share `&counter` — an exclusive `*Counter`, but the *field*
    // they touch is atomic, so concurrent access is well-defined.
    const a = try exec.spawn(bump, .{ &counter, @as(u64, 100_000) });
    const b = try exec.spawn(bump, .{ &counter, @as(u64, 100_000) });
    a.join();
    b.join();

    try out.print("total = {d}\n", .{counter.value.load(.monotonic)});
}
```

### 2.3 Thread pools

`std.Thread.Executor` *is* a pool: it reuses a fixed set of worker threads
rather than spawning a fresh OS thread per unit of work. Sizing and submission
are explicit, and the executor is a capability value you create once and pass
down. A larger fan-out uses `submit` plus `waitIdle`:

```k2
const std = @import("std");

fn task(out: anytype, id: u32) void {
    out.print("task {d} ran\n", .{id}) catch {};
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // Build an executor over the heap capability (it allocates its worker
    // array and queue once, up front — visibly, via `init`).
    var exec = try std.Thread.Executor.init(sys.heap, .{ .worker_count = 4 });
    // The executor owns OS threads and a queue; `deinit` joins all workers.
    defer exec.deinit();

    var id: u32 = 0;
    while (id < 8) : (id += 1) {
        // Enqueue work. The executor is a *capability value*; passing `exec` to
        // a function grants exactly the ability to submit tasks, nothing more.
        try exec.submit(task, .{ out, id });
    }

    // `waitIdle` blocks until the queue is drained. Structured: the executor
    // does not outlive this scope, and `deinit` guarantees every worker joins.
    exec.waitIdle();
}
```

Note that `Executor.init` takes the `Allocator` *and* a configuration record
explicitly: an executor both spawns OS threads and allocates its internal
state, so its signature names the heap power it consumes. This is the
capability model paying off — there is no surprise allocation and no surprise
OS access hidden inside `init`. The only authority an `Executor` exercises is
exactly what you handed its constructor.

---

## 3. The `*T` / `*const T` data-race discipline

k2 has no borrow checker. Data-race freedom is instead a **discipline**
expressed through the pointer types already defined in §3 of the memory
chapter, reinforced by the safety allocator's tooling.

The contract is:

| Pointer | Meaning | Concurrency reading |
| --- | --- | --- |
| `*T` | exclusive, mutable access | **at most one** thread may hold and use this at a time |
| `*const T` | shared, read-only access | **many** threads may read this concurrently |

This is the same `*T` versus `*const T` distinction you use for ordinary
single-threaded code, given a concurrent reading:

- A `*const T` may be **freely shared** across threads. Many readers of
  immutable data never race; concurrent reads of memory that nobody writes are
  always safe.
- A `*T` carries an **exclusivity obligation**. If you hand the same `*T` to
  two threads and both write through it without synchronization, that is a data
  race and therefore undefined behavior in ReleaseFast (and very likely a
  use-after-free or torn-write the safety allocator flags in Debug/ReleaseSafe).

The discipline, stated as a rule:

> **To share mutable state across threads, do not share a bare `*T`. Share a
> pointer to a value whose mutation is synchronized** — an atomic
> (`std.atomic.Value(T)`), or data guarded by a `Mutex` — so that the
> *exclusivity* the `*T` promises is actually maintained at runtime.

`Counter` in §2.2 obeys this: the threads share `*Counter`, but the *field*
they mutate is a `std.atomic.Value(u64)`, so the exclusivity obligation is
discharged atomically rather than by a lock. §5's `Mutex` example shows the
other half: ordinary `*T` data made safe to share by wrapping access in a lock.

Because this is a convention rather than a checked property, k2 leans on the
safety builds: in Debug and ReleaseSafe, the runtime-checked
`GeneralPurposeAllocator` and the safety-check pass surface many race symptoms
(use-after-free across threads, torn writes that corrupt allocator metadata) as
deterministic panics. They are not a *proof* of race freedom — they are a net
that catches the common mistakes early. The proof is your discipline.

---

## 4. Atomics and memory ordering

Atomics are provided by `std.atomic`, not by `@`-builtins: keeping them in the
library matches the small-surface pillar and the "capabilities and primitives
are plain values" stance. An atomic is a typed wrapper whose operations are
indivisible with respect to other threads.

### 4.1 `std.atomic.Value(T)`

`std.atomic.Value(T)` is a generic — a comptime function of a type, exactly
like every other generic in k2 (§14 of the type chapter). It wraps an integer,
boolean, or pointer-sized `T` and exposes atomic operations.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    var flag = std.atomic.Value(bool).init(false);
    var count = std.atomic.Value(u64).init(0);

    // load / store with an explicit ordering — never a hidden default.
    flag.store(true, .release);
    const seen = flag.load(.acquire);

    // read-modify-write returns the PREVIOUS value, as one atomic step.
    const prev = count.fetchAdd(1, .monotonic);

    const out = sys.io.stdout();
    try out.print("seen={any} prev={d}\n", .{ seen, prev });
}
```

The core operations, each of which takes an explicit ordering:

| Operation | Meaning |
| --- | --- |
| `load(order)` | atomically read the value |
| `store(value, order)` | atomically write the value |
| `swap(value, order)` | store and return the previous value |
| `fetchAdd` / `fetchSub` | add/subtract, return previous value |
| `fetchAnd` / `fetchOr` / `fetchXor` | bitwise RMW, return previous value |
| `cmpxchgStrong(expected, new, success_order, fail_order)` | compare-and-swap; returns `?T` — `null` on success, the actual value on failure |
| `cmpxchgWeak(...)` | as above, may fail spuriously; for retry loops |

### 4.2 Memory ordering

The ordering argument is a `std.atomic.Ordering` enum. It is **required** on
every atomic operation — there is no implicit "sequentially consistent by
default" hidden behind your back, because that would be hidden control flow
over the memory model. You state the ordering you need, and pay for exactly
that.

```k2
const std = @import("std");

pub const Ordering = enum {
    /// No ordering guarantees beyond atomicity of this one operation.
    /// Cheapest. Correct for standalone counters/tallies.
    monotonic,
    /// A load that acquires: no later memory access in this thread can be
    /// reordered before it. Pairs with `release`.
    acquire,
    /// A store that releases: no earlier memory access in this thread can be
    /// reordered after it. Pairs with `acquire`.
    release,
    /// Both an acquire and a release on the same RMW operation.
    acq_rel,
    /// A single global total order across all `seq_cst` operations.
    /// The strongest and most expensive; the safe default when unsure.
    seq_cst,
};
```

The canonical acquire/release handshake — publish data with a `release` store,
observe it with an `acquire` load — is the bread-and-butter pattern:

```k2
const std = @import("std");

const Mailbox = struct {
    payload: u64,                       // plain data, written before publish
    ready: std.atomic.Value(bool),      // the publication flag
};

/// Producer: fill `payload`, then publish with a RELEASE store.
fn produce(mb: *Mailbox, value: u64) void {
    mb.payload = value;                 // (1) ordinary write
    mb.ready.store(true, .release);     // (2) release: (1) is visible to any
                                        //     thread that acquires `ready`
}

/// Consumer: spin on an ACQUIRE load, then read `payload` safely.
fn consume(mb: *Mailbox) u64 {
    while (mb.ready.load(.acquire) == false) {
        // busy-wait; a real program would yield or use a condition variable
    }
    return mb.payload;                  // guaranteed to see the producer's write
}

pub fn main(sys: *System) !void {
    var exec = try std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    const out = sys.io.stdout();

    var mb = Mailbox{ .payload = 0, .ready = std.atomic.Value(bool).init(false) };

    const p = try exec.spawn(produce, .{ &mb, @as(u64, 99) });
    defer p.join();

    const got = consume(&mb);
    try out.print("consumed = {d}\n", .{got});
}
```

The `release`/`acquire` pair is what makes the *non-atomic* write to
`payload` safe to read on the other thread: the release store and the acquire
load synchronize, and everything sequenced before the store on the producer is
visible after the load on the consumer. This is the whole reason ordering must
be explicit — it is the only thing standing between you and a data race on
`payload`.

### 4.3 Compare-and-swap

`cmpxchg*` returns `?T`: `null` means the swap succeeded; a non-null value is
the actual contents that did not match `expected`, which you feed into the next
retry. This is the lock-free idiom for building larger atomic operations out of
a CAS loop.

```k2
const std = @import("std");

/// Atomically clamp-add: increase `*v` by `delta` but never above `max`.
/// Returns the new value. Lock-free via a compare-and-swap retry loop.
fn addClamped(v: *std.atomic.Value(u64), delta: u64, max: u64) u64 {
    var current = v.load(.monotonic);
    while (true) {
        var next = current + delta;
        if (next > max) next = max;
        // Try to publish `next` only if nobody changed `current` meanwhile.
        const witness = v.cmpxchgWeak(current, next, .acq_rel, .monotonic);
        if (witness) |actual| {
            current = actual;   // someone raced us; retry with the real value
        } else {
            return next;        // success: cmpxchg returned null
        }
    }
}
```

---

## 5. Synchronization primitives

For state that is not a single word, you guard it with a lock. `std` ships the
primitives; there is **no implicit locking** anywhere in the language or
standard library.

### 5.1 `std.Thread.Mutex`

A `Mutex` provides mutual exclusion. The idiom pairs `lock()` with
`defer mutex.unlock()` so the unlock is guaranteed on every exit path — the
same `defer` discipline used for allocations, applied to a lock. There are no
implicit destructors, so the unlock is written explicitly and is plainly
visible.

```k2
const std = @import("std");

/// A bank balance protected by a mutex. The mutex guards `balance`; the rule
/// is "never touch `balance` without holding `mu`".
const Account = struct {
    mu: std.Thread.Mutex,
    balance: i64,

    fn init() Account {
        return Account{ .mu = std.Thread.Mutex{}, .balance = 0 };
    }

    fn deposit(self: *Account, amount: i64) void {
        self.mu.lock();
        // The unlock runs on EVERY exit from this scope — no hidden control
        // flow, just a scheduled statement.
        defer self.mu.unlock();
        self.balance += amount;
    }

    fn read(self: *Account) i64 {
        self.mu.lock();
        defer self.mu.unlock();
        return self.balance;
    }
};

fn depositMany(acct: *Account, n: i64) void {
    var i: i64 = 0;
    while (i < n) : (i += 1) {
        acct.deposit(1);
    }
}

pub fn main(sys: *System) !void {
    var exec = try std.Thread.Executor.init(sys.heap, .{ .worker_count = 2 });
    defer exec.deinit();
    const out = sys.io.stdout();

    var acct = Account.init();

    const a = try exec.spawn(depositMany, .{ &acct, @as(i64, 50_000) });
    const b = try exec.spawn(depositMany, .{ &acct, @as(i64, 50_000) });
    a.join();
    b.join();

    try out.print("balance = {d}\n", .{acct.read()});  // 100000, no races
}
```

Note how the `Account` example reconciles with §3: the threads share a `*T`
(`*Account`), but every mutation of `balance` happens under the lock, so the
exclusivity the `*T` promises is genuinely maintained — only one thread is ever
inside the critical section.

### 5.2 The rest of the toolbox

`std.Thread` provides the standard kit; each is an ordinary value, never an
ambient global:

- **`std.Thread.RwLock`** — many concurrent readers *or* one writer; for
  read-mostly shared state.
- **`std.Thread.Condition`** — a condition variable to block until a predicate
  becomes true, paired with a `Mutex`; the building block for queues and
  bounded buffers without busy-waiting.
- **`std.Thread.Semaphore`** — counting semaphore for bounding concurrency
  (e.g. "at most N in flight").
- **`std.Thread.WaitGroup`** — wait for a set of spawned tasks to complete; the
  structured-concurrency join primitive (§6).
- **`std.atomic`** — the lock-free layer from §4, underneath all of the above.

All of these are *library types built on the OS-thread `Executor` capability
and the atomics in §4.* None of them is special language machinery, and none of
them reaches the OS without an executor (and thus a heap capability) having been
threaded in to create it.

---

## 6. Structured concurrency

k2 does not bake a structured-concurrency framework into the language, but its
existing primitives — capabilities, `defer`/`errdefer`, and explicit join —
make the *structured* style the natural one. The discipline is:

> **A spawned unit of work does not outlive the scope that spawned it.** The
> scope owns the handle (or the executor), and `defer` guarantees the join on
> every exit path — including the error path.

This falls straight out of the rules already established: a task handle is a
resource, resources are released with `defer`, and `defer` runs on every exit.
There is no detached, ambient task drifting loose with no owner.

```k2
const std = @import("std");

/// Run `work` on N tasks and wait for all of them before returning.
/// Structured: every task spawned here is joined here, on every path.
fn parallelFill(exec: anytype, out: []u64) !void {
    var handles: [4]std.Thread.Task = undefined;
    var spawned: usize = 0;
    // If a later spawn fails, errdefer joins the ones we already started so we
    // never leak a running task out of this function.
    errdefer {
        var j: usize = 0;
        while (j < spawned) : (j += 1) handles[j].join();
    }

    const chunk = out.len / handles.len;
    for (handles[0..], 0..) |*h, i| {
        const lo = i * chunk;
        const hi = if (i == handles.len - 1) out.len else lo + chunk;
        h.* = try exec.spawn(fillRange, .{ out, lo, hi });
        spawned += 1;
    }

    // Success path: join all. The `out` slice is fully written by the time we
    // return — no task escapes this scope.
    for (handles[0..]) |*h| h.join();
}

fn fillRange(out: []u64, lo: usize, hi: usize) void {
    var i = lo;
    while (i < hi) : (i += 1) {
        const v: u64 = @intCast(i);
        out[i] = v * v;   // disjoint ranges: no two threads touch the same slot
    }
}

pub fn main(sys: *System) !void {
    const alloc = sys.heap;
    const out = sys.io.stdout();

    var exec = try std.Thread.Executor.init(alloc, .{ .worker_count = 4 });
    defer exec.deinit();

    const data: []u64 = try alloc.alloc(u64, 1024);
    defer alloc.free(data);

    try parallelFill(&exec, data);
    try out.print("data[1000] = {d}\n", .{data[1000]});
}
```

Two things make this *structured*:

1. **Disjoint mutation, no sharing of a written `*T`.** Each task writes a
   distinct range of the same slice. No two threads touch the same element, so
   there is no race despite there being no lock — the §3 discipline is honored
   by *partitioning* rather than by synchronization.
2. **`errdefer` on the half-spawned set.** If the third `spawn` fails after two
   succeeded, the `errdefer` joins the two live tasks before the error
   propagates. A partially-built fan-out is unwound exactly like a half-built
   heap resource (§6 of the memory chapter) — same idiom, applied to tasks.

This is the whole of k2's structured-concurrency story: it is not a new
language feature, it is the *consistent application of the cleanup primitives
you already have* to task handles and executors.

---

## 7. Async: colorless, stackless, no runtime, no keywords

k2 has an async model, and it is deliberately **not** a runtime and **not** a
pair of keywords. The charter is explicit: async is *"a colorless, stackless
async model lowered at compile time."* Crucially, k2 ships **no `async` or
`await` keywords** — the locked 35-keyword set does not contain them, and the
grammar has no async/await production. The model is therefore expressed as
**ordinary method calls on an event-loop capability**. Three words carry the
design.

- **Lowered at compile time.** Starting an async call is a *transformation*,
  not a scheduler. The compiler rewrites the called function into a state
  machine: each suspension point becomes a state, and the function's locals that
  must survive a suspend become fields of a `Frame` struct. This is the same
  kind of monomorphizing transform as generics — it happens in the compiler, and
  the result is ordinary machine code.
- **Stackless.** An async call does **not** get its own OS stack. Its suspended
  state is the `Frame` struct and nothing more — no separate call-stack region,
  no stack-switching. A `Frame` is a value with a known, comptime-computable
  size.
- **Colorless.** A function is not painted "async" in its type the way it is in
  many languages, and there is no viral keyword in every signature up the call
  chain. Whether a call suspends is determined by *how it is started* and *what
  capability it is handed*, not by a keyword. The same function can run blocking
  or evented depending on the event-loop capability it receives (§8).

> **No new keywords, no new operators.** Where another language writes
> `async f(...)` and `await frame`, k2 writes `loop.spawn(f, .{ ... })` and
> `frame.await(loop)` — a constructor-style method that returns a `Frame`, and a
> method on that `Frame` that drives it to completion. Both are *ordinary
> function calls*, lexed and parsed by the existing grammar, with no entry in
> the keyword set and no new `unary_op`. (`await` here is a **method name**, not
> a keyword; it sits in identifier position exactly like `init` or `deinit`.)

### 7.1 The `Frame` and who owns its storage

The defining property — and k2's whole reason for having async at all without a
runtime — is that **the caller owns the frame's storage. There is no hidden
allocation.** A `loop.spawn` call returns a `Frame` value; you decide where it
lives (stack, arena, an embedded field), exactly as you decide where any value
lives.

```k2
const std = @import("std");

/// An ordinary fallible function. It may suspend at the `loop.readU64` points
/// it itself awaits. Note there is no special return-type painting — it returns
/// `!u64` like any other fallible fn, and takes the loop as a plain parameter.
fn fetchAndSum(loop: anytype, a: []const u8, b: []const u8) !u64 {
    // Each `loop.readU64(...).await(loop)` is a potential suspension point —
    // an ordinary call. Between them this runs as straight-line code.
    const x = try loop.readU64(a).await(loop);
    const y = try loop.readU64(b).await(loop);
    return x + y;
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    // The event loop is a capability you construct from the heap and pass
    // explicitly — never ambient (see §8).
    var loop = try std.event.Loop.init(sys.heap);
    defer loop.deinit();

    // `loop.spawn` starts the call and returns a Frame. The Frame's storage is
    // `frame` — a stack local in `main`. NOTHING was heap-allocated for it.
    var frame = loop.spawn(fetchAndSum, .{ &loop, "a.dat", "b.dat" });

    // We can do other work here while the frame is suspended on I/O...

    // `frame.await(loop)` drives the frame to completion and yields its result.
    // The `!` result is unwrapped with `try`, exactly like a synchronous call.
    const total = try frame.await(&loop);
    try out.print("total = {d}\n", .{total});
}
```

What this buys you, consistent with the rest of k2:

- **No hidden allocation.** `frame` lives wherever you put it. If you want many
  frames, you allocate an array of them *visibly*, with an `Allocator` you were
  handed. The language never heap-allocates a frame behind your back.
- **No hidden control flow.** Starting and awaiting a frame are *ordinary
  function calls*, so suspension happens **inside a call** — and a function call
  is the one place the charter already permits control to leave the current line
  ("a function call looks like a function call"). There is no new operator and
  no implicit yield: no auto-await, no continuation captured behind your back. A
  suspension point is always a `.await(loop)` call you can see, and it leaves
  the closed control-flow set
  ([expressions chapter §11](03-expressions-and-statements.md#11-the-closed-set-restated))
  exactly as it found it — async adds nothing to that set.
- **No runtime.** There is no thread that secretly polls your frame. A frame
  makes progress only when something *drives* it — and the thing that drives it
  is the event loop, which you must hand in. That is §8.

### 7.2 Concurrency of frames

Because a frame is just a value you own, running several I/O operations
concurrently is *holding several frames and awaiting them*, with all storage
visible:

```k2
const std = @import("std");

fn run(sys: *System) !void {
    var loop = try std.event.Loop.init(sys.heap);
    defer loop.deinit();
    const out = sys.io.stdout();

    // Three independent async operations, three frames on this stack. They
    // make progress as the loop services their I/O — concurrently, on ONE
    // OS thread, with zero hidden allocation.
    var f0 = loop.spawn(loop.readU64, .{"x.dat"});
    var f1 = loop.spawn(loop.readU64, .{"y.dat"});
    var f2 = loop.spawn(loop.readU64, .{"z.dat"});

    const sum =
        (try f0.await(&loop)) + (try f1.await(&loop)) + (try f2.await(&loop));
    try out.print("sum = {d}\n", .{sum});
}
```

This is concurrency *without parallelism*: one OS thread, three in-flight
operations, interleaved by the event loop at the explicit `.await(...)` calls.
To add parallelism, you combine async with the `Executor` of §2 — but the two
are orthogonal mechanisms, each visible and each capability-gated.

---

## 8. The event loop is an explicit capability

A `Frame` is inert. It advances only when something *drives* it, and that
driver is an **event loop** — and in k2 the event loop is a capability you
**construct from `sys.heap`** and pass explicitly, never an ambient global and
never a hidden field of `*System`.

```k2
const std = @import("std");

pub fn main(sys: *System) !void {
    // The event loop is built from the heap capability. A function that is NOT
    // handed `loop` cannot drive frames — its signature proves it.
    var loop = try std.event.Loop.init(sys.heap);
    defer loop.deinit();
    const out = sys.io.stdout();

    var frame = loop.spawn(serveOnce, .{&loop});
    // Driving the frame to completion may require the loop to process I/O
    // readiness; `frame.await(loop)` cooperates with the loop it was started
    // under.
    try frame.await(&loop);

    try out.print("served one request\n", .{});
}

fn serveOnce(loop: anytype) !void {
    const conn = try loop.accept().await(loop);
    defer conn.close();
    const req = try conn.read().await(loop);
    try conn.write(req);
}
```

This is the crux of the whole design and the reason k2 can have async at all
without contradicting `no runtime` and `no ambient authority`:

- **The same function runs blocking or evented depending on the capability it
  is handed.** Hand `serveOnce` a *blocking* I/O capability whose `.await(...)`
  resolves by blocking the calling thread, and each step resolves immediately.
  Hand it an *event-loop-backed* capability and each `.await(...)` suspends the
  frame and yields to the loop. The function body is identical — colorless. This
  is why async is not a viral keyword: the *capability*, not the signature,
  decides the execution strategy.
- **There is no implicit scheduler.** The loop runs only when your code calls
  into it (directly, or via `.await(...)` on a frame started under it). No
  background thread polls it; no runtime is linked in to tick it. If you never
  drive the loop, nothing happens — there is no action at a distance.
- **It is sandboxable and testable.** Because the loop is a value you construct
  and pass, a test can hand a *fake* loop that resolves operations from an
  in-memory script, deterministically, with no real I/O — the same way a fake
  clock and seeded RNG make the rest of k2 deterministic (see §9 and the
  capability chapter).

> **The honest signature still holds.** `serveOnce(loop: anytype)` cannot reach
> the network on its own — it can only do what `loop` permits. An async
> function is no more privileged than a synchronous one; it simply has an
> event-loop capability among its arguments. Async did not punch a hole in the
> capability model, and it added no keyword to the language.

### 8.1 Realizations: VM fibers vs the native backend (v0.11 note)

The model above is the **language contract**; how it is *executed* depends on the
backend. k2 v0.11 ships a development/test interpreter (`k2c run`), and that
interpreter realizes the colorless async / `Executor` surface with a
**deterministic cooperative fiber scheduler** inside the VM:

- Each spawned unit of work is a **green fiber** with its own call-frame stack;
  a single-threaded **event loop** interleaves ready fibers at the explicit yield
  points (`spawn`, channel `send`/`recv`, `Mutex` acquire, `await`, an explicit
  `yield`). A FIFO ready queue plus FIFO waiter lists make the interleaving — and
  therefore the program's output — **reproducible run to run**.
- An "all fibers blocked" state (the ready queue empties while live fibers
  remain) is a **clean deadlock diagnostic**, reported immediately rather than
  hanging, because every block reason carries an explicit waker.
- Memory `Ordering` is **accepted and ignored** on the single-threaded VM (an RMW
  cannot be interleaved mid-operation, so it is trivially atomic), while the cost
  model stays honest: you still write the ordering you would pay for natively.

The **native backend (post-0.13)** realizes the *same surface* differently: async
lowers to the stackless `Frame` state machine of §7 (no green-thread runtime is
linked into the binary, per §1 and §10), `Executor` maps to **OS threads**, and
`Ordering` emits **real memory fences**. The VM's fiber scheduler is therefore an
implementation detail of the interpreter — **not** a baked-in runtime — and the
charter's "no green threads linked into your binary" commitment holds for shipped
code. Across both realizations the API is identical: capability-passed, keyword-
free, caller-owned-handle. (Concretely, the v0.11 std surface is
`std.Thread.Executor`/`Task`, `std.Channel(T)`, `std.Thread.Mutex`/`WaitGroup`,
`std.atomic.Value(T)`, and `std.event.Loop`/`Future` — every one a value built
from `sys.heap` and passed explicitly, never a global.)

---

## 9. Testing concurrent code

The payoff of capability-passing is that concurrent code is testable *without*
real threads, real clocks, or real I/O. You hand the code under test a mock
capability and assert on the result deterministically. `test` is a first-class
keyword (it is in the charter's keyword set), and a test receives the same kind
of injected capabilities a `main` does.

```k2
const std = @import("std");

/// Pure logic that happens to use the event loop; no OS thread is required to
/// test its single-threaded behavior, and a fake loop makes the async path
/// deterministic.
fn accumulate(loop: anytype, inputs: []const []const u8) !u64 {
    var total: u64 = 0;
    for (inputs) |name| {
        total += try loop.readU64(name).await(loop);
    }
    return total;
}

test "accumulate sums what the fake loop yields" {
    // A FAKE event loop: `readU64` returns scripted values, no real I/O, no
    // scheduler, no thread. Frames are driven inline, deterministically.
    var loop = std.testing.FakeLoop.init(&.{ 10, 20, 12 });

    const total = try accumulate(&loop, &.{ "a", "b", "c" });
    try std.testing.expectEqual(@as(u64, 42), total);
}
```

The same technique covers threaded code: construct an `Executor` backed by an
*inline* strategy (one that runs each "spawned" task synchronously on submit) to
test logic without real concurrency, then build a real OS-thread `Executor` in
integration tests where you actually want parallelism. Determinism and
auditability are not bolted on — they are what capability-passing *is*.

---

## 10. v1 vs future

k2 is honest about what is settled and what is still being designed. This
section is normative about the boundary.

### Settled for v1

- **OS threads via an explicit `Executor` capability** built from `sys.heap`:
  `std.Thread.Executor.init`, `spawn`, `submit`, `join`/`detach`, `waitIdle`.
  (§2)
- **The `*T`/`*const T` data-race discipline** — exclusive vs shared, enforced
  by convention plus the safety allocator's tooling in Debug/ReleaseSafe. No
  borrow checker, by design. (§3)
- **`std.atomic`** with `Value(T)`, the full RMW set, `cmpxchg*`, and an
  explicit, mandatory `Ordering` argument on every operation. (§4)
- **Synchronization primitives** in `std.Thread`: `Mutex`, `RwLock`,
  `Condition`, `Semaphore`, `WaitGroup`. No implicit locking. (§5)
- **Structured concurrency by convention** — `defer`/`errdefer` join, scopes
  own their spawned work. No detached ambient tasks. (§6)
- **No `async`/`await` keywords, and a locked keyword set.** The async model
  adds nothing to the 35-keyword set or to the closed control-flow set: it is
  ordinary method calls (`loop.spawn`, `frame.await(loop)`) on a constructed
  capability. This is a *committed* property, not a TBD. (§§7–8)
- **The capability model applied to concurrency end to end**: executors and
  event loops are library objects built from `sys.heap`; none is an ambient
  global and none is a new `*System` field. (§§2, 8)

### Designed, stabilizing toward v1

- **The colorless, stackless async lowering** with caller-owned `Frame` storage
  and an explicit event-loop capability (§§7–8). The *model* is fixed by the
  charter — no hidden allocation, no scheduler, capability-driven, **no new
  keywords** — and is a committed language deliverable. What is still being
  finalized is the **surface API**: the exact method set of `std.event.Loop`
  (e.g. the spelling and signatures of `spawn`, `await`, and the per-operation
  methods like `readU64`/`accept`) and how blocking-vs-evented strategies are
  selected. The concrete method names shown in §§7–9 are illustrative and may be
  renamed before v1 freezes them; what will **not** change is that async is
  library-shaped, keyword-free, and caller-owns-the-frame.

### Explicitly out of scope (now and intended for the long term)

- **A built-in green-thread scheduler / async runtime.** Rejected by charter: a
  baked-in scheduler is both a runtime and an ambient effect, violating the
  `no runtime` and `no ambient authority` pillars. k2 will not ship one.
- **`async`/`await` (or `suspend`/`resume`) as language keywords.** Rejected by
  the locked-keyword and small-surface pillars: the model is expressed with
  ordinary calls instead, so the 35-keyword set stays genuinely complete.
- **Automatic, language-level data-race prevention** (a borrow checker or an
  effect system for `Send`/`Sync`-style marker enforcement). Rejected for the
  small-surface pillar. Race freedom stays a discipline backed by safe-build
  tooling.
- **Implicit parallelism** (auto-parallelizing loops, hidden work-stealing).
  Parallelism is an effect; it is always written explicitly and gated by a
  capability.

When a feature is not in the "settled" list above, treat it as subject to
change and pin your toolchain version accordingly.

---

## 11. Summary

- k2 has **no concurrency runtime**: no green-thread scheduler, no built-in
  executor, no language runtime in your binary. Concurrency is
  library-provided over OS threads, and every piece of it is a capability or a
  plain value.
- **Threads are reached through an explicit `Executor` capability** that you
  build from `sys.heap` with `std.Thread.Executor.init`. `spawn` returns an
  error union, a handle is a resource closed with `join`/`detach`, and the
  executor reuses workers. There is **no `sys.threads` field** and no global
  `spawn`; the six `*System` capabilities (`heap`, `io`, `clock`, `random`,
  `env`, `net`) are still the only doors to the OS.
- **Data-race freedom is a discipline**, not a borrow checker: `*T` is
  exclusive, `*const T` is shared, and mutable sharing must go through an
  atomic or a lock. The safe-build allocator catches the common mistakes.
- **Atomics live in `std.atomic`** with an **explicit, mandatory memory
  ordering** on every operation — `monotonic`, `acquire`, `release`,
  `acq_rel`, `seq_cst` — because the ordering is part of the visible cost model,
  never a hidden default.
- **Synchronization primitives** (`Mutex`, `RwLock`, `Condition`, `Semaphore`,
  `WaitGroup`) are ordinary `std` values; the `lock()` + `defer unlock()` idiom
  reuses the same explicit-cleanup discipline as allocations. There is no
  implicit locking.
- **Structured concurrency** is not a new feature but the consistent
  application of `defer`/`errdefer` and explicit join to task handles: spawned
  work does not outlive its scope.
- **Async is a compile-time state-machine transformation with no keywords.**
  `async` and `await` are **not** in k2's locked keyword set; an async call is
  the method `loop.spawn(f, .{ ... })` returning a `Frame` whose storage the
  **caller owns** (no hidden allocation), and `frame.await(loop)` drives it.
  Because both are ordinary function calls, suspension happens *inside a call*
  and the closed control-flow set is preserved unchanged.
- **The event loop is an explicit capability** built from `sys.heap`. The same
  function runs blocking or evented depending on the loop it is handed; nothing
  drives a frame unless your code drives the loop. This is what lets k2 have
  async *without* a runtime, *without* ambient authority, and *without* new
  keywords.

In one sentence: **every thread, every lock, every barrier, and every
suspension is an ordinary call you wrote and can see, driven by a capability you
were handed — concurrency with total control and zero hidden machinery.**

---

### See also

- **§1 — Lexical structure:** the locked 35-keyword set that, by design, does
  **not** contain `async`/`await`/`suspend`/`resume` — the reason this chapter's
  async model is library-shaped.
- **§3 — Expressions and statements:** the closed control-flow set
  (`defer`, `errdefer`, `while`, `for`) reused here for joins and cleanup, and
  the rule that suspension stays inside ordinary calls rather than extending it.
- **§5 — Memory and allocators:** the `*T`/`*const T` pointer semantics and the
  `defer`/`errdefer` half-built-resource idiom that structured concurrency
  reuses for task handles, plus the `Allocator` every executor and event loop
  is built from.
- **§6 — Error handling:** `try`/`catch` on `spawn` and on awaited frames;
  errors-as-values has the same cost model on the async path as anywhere else.
- **§10 — The standard library:** the six `*System` capabilities, the
  `Executor` through which `std.Thread`/`std.atomic` touch the world, and the
  `std.event.Loop` and `std.Thread.Executor` library types this chapter builds
  from `sys.heap`.
