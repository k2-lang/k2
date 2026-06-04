//! End-to-end execution tests for the v0.11 cooperative concurrency model: the
//! deterministic fiber scheduler, the `Executor`/`Task` spawn-join surface,
//! channels (order + close), the cooperative `Mutex`, atomics (`fetchAdd`/CAS),
//! async/await via the event loop, the clean deadlock diagnostic (never a hang),
//! and run-to-run determinism. Every test asserts exact stdout / outcome via the
//! captured runner, so a buggy fiber can never spawn a process or abort the host.

use k2_mir::{lower_program, BuildMode};
use k2_parse::{parse, ParseResult};
use k2_resolve::resolve_file;
use k2_syntax::{Expr, Item, SourceFile};
use k2_types::check_file;
use k2_vm::{run_captured, run_metered, RunArgs, RunOutcome};

/// Parses `source` together with the bundled std (mirroring the `k2c` CLI's
/// `parse_program`): the std body is appended and every `const X = @import("std")`
/// is re-pointed at the synthetic std root, so `std.Thread.*`/`std.Channel`/… are
/// real compiled declarations rather than unresolved intrinsics.
fn parse_with_std(source: &str) -> ParseResult {
    let mut combined = String::with_capacity(source.len() + k2_std::STD_BODY.len() + 64);
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());
    let mut result = parse(&combined);
    rewrite_std_imports(&mut result.file);
    result
}

/// Re-points every `const X = @import("std")` to the synthetic std root.
fn rewrite_std_imports(file: &mut SourceFile) {
    for item in &mut file.items {
        if let Item::Const { value, .. } = item {
            if import_target(value).as_deref() == Some("std") {
                let span = value.span();
                *value = Expr::Ident {
                    name: k2_std::STD_ROOT_NAME.to_string(),
                    span,
                };
            }
        }
    }
}

/// If `e` is exactly `@import("name")`, returns the imported name.
fn import_target(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                return Some(text.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Lowers a source string (with the std prelude injected) to a `MirProgram` under
/// `mode`, asserting the front-end stages are clean.
fn lower(src: &str, mode: BuildMode) -> k2_mir::MirProgram {
    let pres = parse_with_std(src);
    assert!(pres.is_ok(), "parse errors: {:?}", pres.diagnostics);
    let resolved = resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve errors: {:?}",
        resolved.diagnostics
    );
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors: {:?}", typed.diagnostics);
    let prog = lower_program(&pres.file, &resolved, typed, mode).expect("lowering must succeed");
    let problems = prog.verify();
    assert!(problems.is_empty(), "malformed MIR: {problems:?}");
    prog
}

/// Runs `src` (Debug) and returns `(stdout, stderr, outcome, exit)`.
fn run(src: &str) -> (String, String, RunOutcome, i32) {
    run_mode(src, BuildMode::Debug)
}

/// Runs `src` under an explicit build mode.
fn run_mode(src: &str, mode: BuildMode) -> (String, String, RunOutcome, i32) {
    let prog = lower(src, mode);
    let (outcome, code, out, err) = run_captured(&prog, RunArgs::new(mode));
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
        outcome,
        code,
    )
}

/// Asserts a program runs successfully and prints exactly `expected`.
fn assert_stdout(src: &str, expected: &str) {
    let (out, err, outcome, code) = run(src);
    assert_eq!(outcome, RunOutcome::Ok, "stderr was: {err}");
    assert_eq!(code, 0, "exit code");
    assert_eq!(out, expected, "stdout mismatch");
}

// =========================================================================
//  (1) spawn / join aggregation
// =========================================================================

#[test]
fn spawn_join_partial_sums_aggregate() {
    // Four fibers compute disjoint partial sums of 0..1000; the aggregate is
    // 499500 regardless of interleaving.
    let src = r#"
const std = @import("std");
fn partialSum(lo: u64, hi: u64) u64 {
    var s: u64 = 0;
    var i = lo;
    while (i < hi) : (i += 1) { s += i; }
    return s;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 4 });
    defer exec.deinit();
    const a = exec.spawn(partialSum, .{ @as(u64, 0), @as(u64, 250) });
    const b = exec.spawn(partialSum, .{ @as(u64, 250), @as(u64, 500) });
    const c = exec.spawn(partialSum, .{ @as(u64, 500), @as(u64, 750) });
    const d = exec.spawn(partialSum, .{ @as(u64, 750), @as(u64, 1000) });
    const total = a.result(u64) + b.result(u64) + c.result(u64) + d.result(u64);
    try out.print("{d}\n", .{total});
}
"#;
    assert_stdout(src, "499500\n");
}

#[test]
fn spawn_join_is_fiber_count_independent() {
    // A single fiber covering the whole range must give the same aggregate as the
    // four-way split above: the result does not depend on fiber count.
    let src = r#"
const std = @import("std");
fn partialSum(lo: u64, hi: u64) u64 {
    var s: u64 = 0;
    var i = lo;
    while (i < hi) : (i += 1) { s += i; }
    return s;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    const a = exec.spawn(partialSum, .{ @as(u64, 0), @as(u64, 1000) });
    try out.print("{d}\n", .{a.result(u64)});
}
"#;
    assert_stdout(src, "499500\n");
}

// =========================================================================
//  (2) channels: producer/consumer order + close
// =========================================================================

#[test]
fn channel_producer_consumer_preserves_order() {
    // A producer sends 1..=10 on a BOUNDED(2) channel (so it blocks on a full
    // buffer, exercising send-park / recv-wake); the consumer collects them in
    // order and the loop terminates when the channel is closed and drained.
    let src = r#"
const std = @import("std");
fn producer(ch: *std.Channel(u64), n: u64) void {
    var i: u64 = 1;
    while (i <= n) : (i += 1) { ch.send(i) catch {}; }
    ch.close();
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    var ch = std.Channel(u64).init(2);
    const p = exec.spawn(producer, .{ &ch, @as(u64, 10) });
    var sum: u64 = 0;
    while (ch.recv()) |v| {
        try out.print("{d} ", .{v});
        sum += v;
    }
    p.join();
    try out.print("| sum={d}\n", .{sum});
}
"#;
    assert_stdout(src, "1 2 3 4 5 6 7 8 9 10 | sum=55\n");
}

#[test]
fn unbounded_channel_transfers_all_values() {
    // An unbounded channel (cap 0): the producer never blocks, the consumer still
    // receives every value in order.
    let src = r#"
const std = @import("std");
fn producer(ch: *std.Channel(u64)) void {
    var i: u64 = 1;
    while (i <= 5) : (i += 1) { ch.send(i * i) catch {}; }
    ch.close();
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    var ch = std.Channel(u64).init(0);
    const p = exec.spawn(producer, .{&ch});
    while (ch.recv()) |v| { try out.print("{d} ", .{v}); }
    p.join();
    try out.print("\n", .{});
}
"#;
    assert_stdout(src, "1 4 9 16 25 \n");
}

// =========================================================================
//  (3) Mutex-protected counter
// =========================================================================

#[test]
fn mutex_counter_ends_at_correct_value() {
    // Eight fibers * 500 bumps each, serialized by a Mutex, must end at 4000.
    let src = r#"
const std = @import("std");
const Counter = struct { mu: std.Thread.Mutex, value: u64 };
fn bump(c: *Counter, times: u64) void {
    var k: u64 = 0;
    while (k < times) : (k += 1) {
        c.mu.lock();
        defer c.mu.unlock();
        c.value += 1;
    }
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 8 });
    defer exec.deinit();
    var counter = Counter{ .mu = std.Thread.Mutex.init(), .value = 0 };
    var handles: [8]std.Thread.Task = undefined;
    var i: usize = 0;
    while (i < 8) : (i += 1) { handles[i] = exec.spawn(bump, .{ &counter, @as(u64, 500) }); }
    i = 0;
    while (i < 8) : (i += 1) { handles[i].join(); }
    try out.print("{d}\n", .{counter.value});
}
"#;
    assert_stdout(src, "4000\n");
}

// =========================================================================
//  (4) atomics: fetchAdd + CAS
// =========================================================================

#[test]
fn atomic_fetch_add_returns_previous() {
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var at = std.atomic.Value(u64).init(10);
    const prev = at.fetchAdd(5, .monotonic);
    try out.print("prev={d} now={d}\n", .{ prev, at.load(.monotonic) });
}
"#;
    assert_stdout(src, "prev=10 now=15\n");
}

#[test]
fn atomic_cas_clamp_loop_reaches_target() {
    // The spec §4.3 lock-free CAS retry: cmpxchg returns null on success, the
    // witnessed value on mismatch.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var at = std.atomic.Value(u64).init(40);
    var cur = at.load(.monotonic);
    while (at.cmpxchgWeak(cur, cur + 2, .acq_rel, .monotonic)) |actual| {
        cur = actual;
    }
    // A failed CAS (wrong expected) returns the actual value.
    const w = at.cmpxchgStrong(0, 99, .seq_cst, .seq_cst);
    if (w) |actual| {
        try out.print("final={d} cas_failed_saw={d}\n", .{ at.load(.monotonic), actual });
    } else {
        try out.print("UNEXPECTED success\n", .{});
    }
}
"#;
    assert_stdout(src, "final=42 cas_failed_saw=42\n");
}

// =========================================================================
//  (5) async / await via the event loop
// =========================================================================

#[test]
fn async_await_yields_correct_values() {
    let src = r#"
const std = @import("std");
fn triple(x: u64) u64 { return x * 3; }
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var loop = std.event.Loop.init(sys.heap);
    defer loop.deinit();
    const f0 = loop.spawn(triple, .{@as(u64, 7)});
    const f1 = loop.spawn(triple, .{@as(u64, 11)});
    const f2 = loop.spawn(triple, .{@as(u64, 100)});
    const sum = f0.await(&loop, u64) + f1.await(&loop, u64) + f2.await(&loop, u64);
    try out.print("{d}\n", .{sum});
}
"#;
    assert_stdout(src, "354\n");
}

// =========================================================================
//  (6) deadlock is reported cleanly (never a hang, never a Rust panic)
// =========================================================================

#[test]
fn deadlock_on_recv_with_no_sender_is_clean() {
    // A recv on a channel that is never sent to / closed, with no other runnable
    // work, is detected the instant the ready queue empties — reported as a clean
    // panic with a nonzero exit, NOT a hang.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var ch = std.Channel(u64).init(0);
    const v = ch.recv();
    if (v) |x| { try out.print("{d}\n", .{x}); }
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "deadlock must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("deadlock")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn deadlock_on_self_relock_is_clean() {
    // A fiber that locks a held mutex again, with no one to release it, deadlocks
    // cleanly rather than hanging.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    var m = std.Thread.Mutex.init();
    m.lock();
    m.lock();
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "self-relock deadlock must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("deadlock")),
        "outcome: {outcome:?}"
    );
}

// =========================================================================
//  (7) WaitGroup
// =========================================================================

#[test]
fn waitgroup_waits_for_all_tasks() {
    // The main fiber blocks on a wait-group until four workers each `done()`.
    let src = r#"
const std = @import("std");
fn worker(wg: *std.Thread.WaitGroup, ch: *std.Channel(u64), id: u64) void {
    ch.send(id) catch {};
    wg.done();
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 4 });
    defer exec.deinit();
    var wg = std.Thread.WaitGroup.init();
    var ch = std.Channel(u64).init(0);
    wg.add(4);
    var id: u64 = 0;
    while (id < 4) : (id += 1) { _ = exec.spawn(worker, .{ &wg, &ch, id }); }
    wg.wait();
    ch.close();
    var total: u64 = 0;
    while (ch.recv()) |v| { total += v; }
    try out.print("total={d}\n", .{total});
}
"#;
    // 0+1+2+3 = 6 (order-independent because we sum).
    assert_stdout(src, "total=6\n");
}

// =========================================================================
//  (8) determinism + ReleaseFast parity
// =========================================================================

#[test]
fn concurrency_example_is_deterministic() {
    // The bundled example, run twice, must produce byte-identical stdout AND an
    // identical instruction count — the strongest signal that the scheduler adds
    // no nondeterminism.
    let src = include_str!("../../../examples/concurrency.k2");
    let prog = lower(src, BuildMode::Debug);
    let (o1, c1, out1, _e1, n1) = run_metered(&prog);
    let (o2, c2, out2, _e2, n2) = run_metered(&prog);
    assert_eq!(o1, RunOutcome::Ok);
    assert_eq!(o2, RunOutcome::Ok);
    assert_eq!(c1, 0);
    assert_eq!(c2, 0);
    assert_eq!(out1, out2, "stdout differs across runs");
    assert_eq!(n1, n2, "instruction count differs across runs");
    assert_eq!(
        String::from_utf8_lossy(&out1),
        "sum 0..1000 = 499500\nrecv: 1 2 3 4 5\ncounter = 4000\natomic = 42\nawait = 42\n"
    );
}

#[test]
fn concurrency_example_matches_in_release_fast() {
    // ReleaseFast only strips safety checks (overflow/leak); the mutex/atomic/
    // channel behavior is identical, so the output matches Debug exactly.
    let src = include_str!("../../../examples/concurrency.k2");
    let (out, _err, outcome, code) = run_mode(src, BuildMode::ReleaseFast);
    assert_eq!(outcome, RunOutcome::Ok);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "sum 0..1000 = 499500\nrecv: 1 2 3 4 5\ncounter = 4000\natomic = 42\nawait = 42\n"
    );
}

// =========================================================================
//  (9) waitIdle / Loop.run drive to QUIESCENCE (not yield-at-most-once)
//
//  `@schedRun` (behind `Executor.waitIdle` and `event.Loop.run`) used to yield
//  at most once: the calling fiber's pc was advanced past the intrinsic before
//  the yield, so on resume it proceeded to the next statement even though spawned
//  / awaited work had not finished. The fix rewinds the pc so `@schedRun`
//  re-executes and re-tests `other_runnable()` each round, draining the ready set
//  to quiescence. These tests assert the *ordering* (drained work is observable
//  BEFORE the drain helper returns), which the old behavior failed.
// =========================================================================

#[test]
fn wait_idle_drains_a_yielding_task_before_returning() {
    // The task yields once before printing its marker. With a real drain,
    // `waitIdle()` returns only after the task's work ran, so "task work" prints
    // BEFORE "waitIdle returned". Under the old yield-at-most-once bug the order
    // was inverted (waitIdle returned first).
    let src = r#"
const std = @import("std");
fn driveWork(out: anytype) void {
    std.Thread.yieldNow();
    out.print("task work\n", .{}) catch {};
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    _ = exec.spawn(driveWork, .{out});
    exec.waitIdle();
    out.print("waitIdle returned\n", .{}) catch {};
}
"#;
    assert_stdout(src, "task work\nwaitIdle returned\n");
}

#[test]
fn loop_run_drives_a_frame_that_awaits_a_child_to_completion() {
    // A frame spawned on the loop awaits a grandchild (which itself yields before
    // completing). `loop.run()` must drive the whole chain to completion before
    // returning, so both inner markers print before "loop.run returned".
    let src = r#"
const std = @import("std");
fn grandchildTask(out: anytype) u64 {
    std.Thread.yieldNow();
    out.print("grandchild ran\n", .{}) catch {};
    return 7;
}
fn childFrame(loop: *std.event.Loop, out: anytype) void {
    const g = loop.spawn(grandchildTask, .{out});
    const v = g.await(loop, u64);
    out.print("frame completed v={d}\n", .{v}) catch {};
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var loop = std.event.Loop.init(sys.heap);
    defer loop.deinit();
    _ = loop.spawn(childFrame, .{ &loop, out });
    loop.run();
    out.print("loop.run returned\n", .{}) catch {};
}
"#;
    assert_stdout(
        src,
        "grandchild ran\nframe completed v=7\nloop.run returned\n",
    );
}

#[test]
fn wait_idle_drains_nested_spawns_to_full_counter() {
    // A spawned task spawns three grandchildren, each of which yields before
    // bumping a shared counter. `waitIdle()` must not return until the whole tree
    // is drained, so the counter reads the full 3 (it read 0 under the old bug:
    // the grandchildren only ran after main returned).
    let src = r#"
const std = @import("std");
const Counter = struct { value: u64 };
fn bumpLeaf(c: *Counter) void {
    std.Thread.yieldNow();
    c.value += 1;
}
fn spawnLeaves(exec: *std.Thread.Executor, c: *Counter) void {
    std.Thread.yieldNow();
    _ = exec.spawn(bumpLeaf, .{c});
    _ = exec.spawn(bumpLeaf, .{c});
    _ = exec.spawn(bumpLeaf, .{c});
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 4 });
    defer exec.deinit();
    var c = Counter{ .value = 0 };
    _ = exec.spawn(spawnLeaves, .{ &exec, &c });
    exec.waitIdle();
    try out.print("counter={d}\n", .{c.value});
}
"#;
    assert_stdout(src, "counter=3\n");
}

#[test]
fn wait_idle_does_not_spin_past_a_permanently_blocked_task() {
    // A drain must drive *ready* work to completion but must NOT loop forever when
    // the only remaining non-current fiber is permanently blocked (no ready
    // waker). Here the spawned task yields, then blocks on a recv that never
    // arrives. `waitIdle()` reaches quiescence (the blocked task is not runnable),
    // returns, main proceeds, and the program then deadlocks CLEANLY on the
    // unsatisfiable handle — never an infinite drain loop, never a hang.
    let src = r#"
const std = @import("std");
fn stuckTask(ch: *std.Channel(u64)) void {
    std.Thread.yieldNow();
    _ = ch.recv();
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var exec = std.Thread.Executor.init(sys.heap, .{ .worker_count = 1 });
    defer exec.deinit();
    var ch = std.Channel(u64).init(0);
    _ = exec.spawn(stuckTask, .{&ch});
    exec.waitIdle();
    try out.print("returned\n", .{});
}
"#;
    let (out, _err, outcome, code) = run(src);
    assert!(
        out.contains("returned"),
        "waitIdle must return (stdout: {out:?})"
    );
    assert_ne!(code, 0, "the eventual deadlock must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("deadlock")),
        "outcome: {outcome:?}"
    );
}

// =========================================================================
//  (10) atomics / narrowing: u64 values above i64::MAX round-trip cleanly
//
//  `std.atomic.Value(u64)` used to clean-panic ("cast truncated value") for any
//  value above i64::MAX, because the guarded narrowing cast back from the i128
//  cell to the monomorphized `T` resolved its target range to a fabricated signed
//  i64 (the `@as(T, …)` carrier never concretized to u64). The fix range-checks a
//  narrow only against a KNOWN sized target; an unknown/unsized target is
//  unconstrained (matching the unmasked `IntNarrow` it guards), so a valid large
//  `u64` no longer traps — while a genuinely out-of-range narrow STILL traps.
// =========================================================================

#[test]
fn atomic_u64_round_trips_value_above_i64_max() {
    // 2^63 (= 9223372036854775808) is the first value the old bug rejected.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var a = std.atomic.Value(u64).init(9223372036854775808);
    try out.print("{d}\n", .{a.load(.seq_cst)});
}
"#;
    assert_stdout(src, "9223372036854775808\n");
}

#[test]
fn atomic_u64_fetch_add_crossing_i64_max_is_correct() {
    // The spec §2.2 unsigned counter: the moment a tally passes 2^63 it must NOT
    // corrupt. init(2^63 - 8) then +10 crosses the boundary to 2^63 + 2.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var a = std.atomic.Value(u64).init(9223372036854775800);
    const prev = a.fetchAdd(10, .monotonic);
    try out.print("prev={d} now={d}\n", .{ prev, a.load(.monotonic) });
}
"#;
    assert_stdout(src, "prev=9223372036854775800 now=9223372036854775810\n");
}

#[test]
fn atomic_u64_swap_and_cas_handle_u64_max() {
    // swap returns the previous value and cmpxchg witnesses u64::MAX
    // (18446744073709551615) intact — both above i64::MAX.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var a = std.atomic.Value(u64).init(0);
    const old = a.swap(18446744073709551615, .seq_cst);
    // A matching CAS succeeds (returns null); the cell becomes 1.
    const w = a.cmpxchgStrong(18446744073709551615, 1, .seq_cst, .seq_cst);
    if (w) |actual| {
        try out.print("UNEXPECTED saw={d}\n", .{actual});
    } else {
        try out.print("old={d} now={d}\n", .{ old, a.load(.seq_cst) });
    }
}
"#;
    assert_stdout(src, "old=0 now=1\n");
}

#[test]
fn intcast_to_unsigned_large_in_range_value_does_not_trap() {
    // The fix is general, not atomic-specific: a direct `@intCast` to `u64` of a
    // large-but-in-range value (here from an i128) round-trips, never traps.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const big: i128 = 9223372036854775808;
    const x: u64 = @intCast(big);
    const top: i128 = 18446744073709551615;
    const y: u64 = @intCast(top);
    try out.print("{d} {d}\n", .{ x, y });
}
"#;
    assert_stdout(src, "9223372036854775808 18446744073709551615\n");
}

#[test]
fn intcast_genuinely_out_of_range_still_traps() {
    // The narrow check is NOT disabled wholesale: a value that genuinely exceeds
    // the KNOWN target's range (2^64 into a u64) still clean-panics with the
    // narrow-loss trap, exiting nonzero — never a silent wrap, never a host abort.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const big: i128 = 18446744073709551616;
    const x: u64 = @intCast(big);
    try out.print("{d}\n", .{x});
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "out-of-range narrow must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("truncated")),
        "outcome: {outcome:?}"
    );
}
