//! End-to-end execution tests: lower a self-contained k2 program to MIR, run it
//! on the VM, and assert its exact stdout / exit behaviour. These are the
//! milestone's "programs RUN" tests — recursion, slice sum, struct math,
//! optionals, error propagation via `try`, `switch`, runtime `defer` ordering,
//! and the safety traps (index OOB / overflow / div-by-zero), each verified
//! against real output.

use k2_mir::{lower_program, BuildMode};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;
use k2_vm::{run_captured, RunArgs, RunOutcome};

/// Lowers a source string to a `MirProgram` under `mode`, asserting the front-end
/// stages are clean (these test programs are written to type-check).
fn lower(src: &str, mode: BuildMode) -> k2_mir::MirProgram {
    let pres = parse(src);
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

/// Runs `src` (Debug mode) and returns `(stdout, stderr, outcome, exit)`.
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

/// Asserts a program runs successfully and prints exactly `expected` to stdout.
fn assert_stdout(src: &str, expected: &str) {
    let (out, err, outcome, code) = run(src);
    assert_eq!(outcome, RunOutcome::Ok, "stderr was: {err}");
    assert_eq!(code, 0, "exit code");
    assert_eq!(out, expected, "stdout mismatch");
}

// =========================================================================
//  The HARD acceptance: examples/hello.k2 exact output.
// =========================================================================

#[test]
fn hello_exact_output() {
    let src = include_str!("../../../examples/hello.k2");
    let (out, err, outcome, code) = run(src);
    assert_eq!(outcome, RunOutcome::Ok);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "Hello, k2!\nk2 directs every joule of Sol: ~384600000000000000000000000 W.\n"
    );
    assert_eq!(err, "(this line went to stderr)\n");
}

// =========================================================================
//  examples/errors.k2 — heap intrinsics + try/catch/errdefer/switch/for.
// =========================================================================

#[test]
fn errors_example_runs() {
    let src = include_str!("../../../examples/errors.k2");
    let (out, err, outcome, code) = run(src);
    assert_eq!(outcome, RunOutcome::Ok, "stderr was: {err}");
    assert_eq!(code, 0);
    assert!(out.contains("doubled(\"21\") = 42"), "out: {out}");
    assert!(
        out.contains("doubled(\"5\") + doubled(\"9\") = 28"),
        "out: {out}"
    );
    assert!(
        out.contains("parseU32(\"123\") = 123, parseU32(\"oops\") = 0"),
        "out: {out}"
    );
    assert!(out.contains("error Empty (code 10)"), "out: {out}");
    assert!(out.contains("error NotANumber (code 11)"), "out: {out}");
    assert!(out.contains("error Overflow (code 12)"), "out: {out}");
}

// =========================================================================
//  Self-contained runnable programs.
// =========================================================================

#[test]
fn recursion_fib() {
    let src = r#"
fn fib(n: u32) u32 {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("fib(10) = {d}\n", .{fib(10)});
}
"#;
    assert_stdout(src, "fib(10) = 55\n");
}

#[test]
fn slice_sum() {
    let src = r#"
fn sumSlice(xs: []const u32) u32 {
    var total: u32 = 0;
    for (xs) |x| {
        total += x;
    }
    return total;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const arr = [_]u32{ 1, 2, 3, 4, 5 };
    try out.print("sum = {d}\n", .{sumSlice(&arr)});
}
"#;
    assert_stdout(src, "sum = 15\n");
}

#[test]
fn struct_field_math() {
    let src = r#"
const Point = struct { x: i32, y: i32 };
fn area(p: Point) i32 {
    return p.x + p.y;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p = Point{ .x = 3, .y = 4 };
    try out.print("m = {d}\n", .{area(p)});
}
"#;
    assert_stdout(src, "m = 7\n");
}

#[test]
fn optional_unwrap_and_orelse() {
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const m: ?i32 = 7;
    const opt = m.?;
    const n: ?i32 = null;
    const got = n orelse 9;
    try out.print("opt = {d}, got = {d}\n", .{ opt, got });
}
"#;
    assert_stdout(src, "opt = 7, got = 9\n");
}

#[test]
fn error_propagation_via_try_success() {
    let src = r#"
const ParseError = error{ Empty, NotANumber };
fn parseU32(text: []const u8) ParseError!u32 {
    if (text.len == 0) return ParseError.Empty;
    var value: u32 = 0;
    for (text) |ch| {
        if (ch < '0' or ch > '9') return ParseError.NotANumber;
        value = value * 10 + @as(u32, ch - '0');
    }
    return value;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const v = try parseU32("123");
    try out.print("v = {d}\n", .{v});
}
"#;
    assert_stdout(src, "v = 123\n");
}

#[test]
fn error_escapes_main_prints_name() {
    let src = r#"
const ParseError = error{ Empty, NotANumber };
fn parseU32(text: []const u8) ParseError!u32 {
    if (text.len == 0) return ParseError.Empty;
    return 0;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const v = try parseU32("");
    try out.print("v = {d}\n", .{v});
}
"#;
    let (out, err, outcome, code) = run(src);
    assert_eq!(out, "", "no stdout before the error");
    assert_ne!(code, 0, "nonzero exit on error escaping main");
    assert!(
        matches!(&outcome, RunOutcome::Errored(name) if name == "Empty"),
        "outcome: {outcome:?}, err: {err}"
    );
}

// An error propagated through several `try` sites out of `main` produces an
// error-return trace listing the ORIGIN (`return Boom.Boom`) plus those sites,
// newest-first (origin deepest/last).
#[test]
fn error_return_trace_lists_origin_and_try_sites_newest_first() {
    let src = r#"
const Boom = error{ Boom };
fn b() Boom!u32 {
    return Boom.Boom;
}
fn a() Boom!u32 {
    const x = try b();
    return x;
}
pub fn main(sys: *System) !void {
    _ = sys;
    const y = try a();
    _ = y;
}
"#;
    let prog = lower(src, BuildMode::Debug);
    let (outcome, code, _out, _err, trace) =
        k2_vm::run_captured_traced(&prog, RunArgs::new(BuildMode::Debug));
    assert!(matches!(&outcome, RunOutcome::Errored(n) if n == "Boom"));
    assert_ne!(code, 0);
    // The trace records the origin (`return Boom.Boom` in b), then the `try b()`
    // (in a) and `try a()` (in main) sites, newest-first: the innermost frame
    // (the origin in `b`) is first; `main`'s `try` is last.
    assert_eq!(trace.len(), 3, "trace frames: {trace:?}");
    assert_eq!(trace[0].fn_name, "b", "origin frame missing/misordered");
    assert_eq!(trace[1].fn_name, "a");
    assert_eq!(trace[2].fn_name, "main");
    // The locations are real source positions (origin + the two `try` sites).
    assert!(trace.iter().all(|f| f.line > 0 && f.col > 0));
}

// A direct `return error.X` with no `try` above it still records its ORIGIN
// frame: the trace is exactly one frame at the creation site.
#[test]
fn error_return_trace_records_origin_with_no_try() {
    let src = r#"
const Boom = error{ Boom };
pub fn main(sys: *System) !void {
    _ = sys;
    return Boom.Boom;
}
"#;
    let prog = lower(src, BuildMode::Debug);
    let (outcome, code, _out, _err, trace) =
        k2_vm::run_captured_traced(&prog, RunArgs::new(BuildMode::Debug));
    assert!(matches!(&outcome, RunOutcome::Errored(n) if n == "Boom"));
    assert_ne!(code, 0);
    assert_eq!(trace.len(), 1, "expected one origin frame: {trace:?}");
    assert_eq!(trace[0].fn_name, "main");
    assert!(trace[0].line > 0 && trace[0].col > 0);
}

// In ReleaseFast the trace machinery is stripped: the error still escapes with
// the right name and exit code, but no trace frames are recorded.
#[test]
fn error_return_trace_stripped_in_release_fast() {
    let src = r#"
const Boom = error{ Boom };
fn b() Boom!u32 {
    return Boom.Boom;
}
fn a() Boom!u32 {
    const x = try b();
    return x;
}
pub fn main(sys: *System) !void {
    _ = sys;
    const y = try a();
    _ = y;
}
"#;
    let prog = lower(src, BuildMode::ReleaseFast);
    let (outcome, code, _out, _err, trace) =
        k2_vm::run_captured_traced(&prog, RunArgs::new(BuildMode::ReleaseFast));
    assert!(matches!(&outcome, RunOutcome::Errored(n) if n == "Boom"));
    assert_ne!(code, 0);
    assert!(
        trace.is_empty(),
        "ReleaseFast must strip the trace: {trace:?}"
    );
}

// A `catch` that handles the error stops propagation: the caught error never
// reaches the escape printer, so a *clean* program prints no trace.
#[test]
fn caught_error_does_not_escape_or_trace() {
    let src = r#"
const Boom = error{ Boom };
fn b() Boom!u32 {
    return Boom.Boom;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const v = b() catch 7;
    try out.print("v = {d}\n", .{v});
}
"#;
    let prog = lower(src, BuildMode::Debug);
    let (outcome, code, out, _err, trace) =
        k2_vm::run_captured_traced(&prog, RunArgs::new(BuildMode::Debug));
    assert_eq!(outcome, RunOutcome::Ok);
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8_lossy(&out), "v = 7\n");
    assert!(trace.is_empty(), "a handled error must not leave a trace");
}

#[test]
fn catch_default_value() {
    let src = r#"
const ParseError = error{ Empty, NotANumber };
fn parseU32(text: []const u8) ParseError!u32 {
    if (text.len == 0) return ParseError.Empty;
    var value: u32 = 0;
    for (text) |ch| {
        if (ch < '0' or ch > '9') return ParseError.NotANumber;
        value = value * 10 + @as(u32, ch - '0');
    }
    return value;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const ok = parseU32("42") catch 0;
    const bad = parseU32("nope") catch 0;
    try out.print("ok = {d}, bad = {d}\n", .{ ok, bad });
}
"#;
    assert_stdout(src, "ok = 42, bad = 0\n");
}

#[test]
fn switch_enum_dispatch() {
    let src = r#"
const Color = enum { red, green, blue };
fn code(c: Color) u8 {
    return switch (c) {
        .red => 1,
        .green => 2,
        .blue => 3,
    };
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{d} {d} {d}\n", .{ code(.red), code(.green), code(.blue) });
}
"#;
    assert_stdout(src, "1 2 3\n");
}

#[test]
fn defer_runs_in_lifo_order() {
    // Two defers print markers; LIFO means "two" prints before "one".
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    defer out.print("one\n", .{}) catch {};
    defer out.print("two\n", .{}) catch {};
    try out.print("body\n", .{});
}
"#;
    assert_stdout(src, "body\ntwo\none\n");
}

#[test]
fn errdefer_only_runs_on_error_path() {
    // `errdefer` fires when the scope exits via an error, and is skipped on
    // success. We exercise both by calling a helper that errors and one that
    // succeeds, and observe the marker only on the error path.
    let src = r#"
const E = error{ Boom };
fn work(out: anytype, fail: bool) E!u32 {
    errdefer out.print("cleanup\n", .{}) catch {};
    if (fail) return E.Boom;
    return 7;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = work(out, false) catch 0;
    try out.print("a = {d}\n", .{a});
    const b = work(out, true) catch 99;
    try out.print("b = {d}\n", .{b});
}
"#;
    // On success no "cleanup"; on error "cleanup" prints before b is reported.
    assert_stdout(src, "a = 7\ncleanup\nb = 99\n");
}

// =========================================================================
//  Runtime safety traps: nonzero exit + a stderr message in Debug; the same
//  program in ReleaseFast does NOT trap (wraps / reads clamped).
// =========================================================================

#[test]
fn trap_index_out_of_bounds() {
    let src = r#"
pub fn main(sys: *System) !void {
    const a = [_]u8{ 1, 2, 3 };
    const i: usize = 9;
    _ = a[i];
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "OOB must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("index out of bounds")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn trap_integer_overflow() {
    let src = r#"
pub fn main(sys: *System) !void {
    var x: u8 = 250;
    const y: u8 = 10;
    x += y;
    _ = x;
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0);
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("integer overflow")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn trap_division_by_zero() {
    let src = r#"
pub fn main(sys: *System) !void {
    const a: u32 = 10;
    const b: u32 = 0;
    _ = a / b;
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0);
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("division by zero")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn release_fast_drops_overflow_check() {
    // The same overflowing add wraps (250 + 10 = 260 -> 4 mod 256) with no trap.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var x: u8 = 250;
    const y: u8 = 10;
    x += y;
    try out.print("x = {d}\n", .{x});
}
"#;
    let (out, _err, outcome, code) = run_mode(src, BuildMode::ReleaseFast);
    assert_eq!(outcome, RunOutcome::Ok, "should not trap in ReleaseFast");
    assert_eq!(code, 0);
    assert_eq!(out, "x = 4\n");
}

#[test]
fn release_fast_drops_bounds_check() {
    // An out-of-bounds index in ReleaseFast must NOT trap on the VM — the bounds
    // check is stripped, matching the native backend (which also strips it and
    // reads OOB without trapping). The VM clamps the read to the last element
    // (a defined, non-panicking value); the exact value of an OOB read is UB and
    // need not match native byte-for-byte, but the no-trap / exit-0 behavior must.
    // (In Debug this same program traps with `index out of bounds` — see
    // `trap_index_out_of_bounds` — so the strip is mode-specific, not a removal of
    // the check everywhere.)
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = [_]u32{ 10, 20, 30, 40 };
    var i: usize = 9;
    const x = a[i];
    try out.print("done {d}\n", .{x});
}
"#;
    let (out, _err, outcome, code) = run_mode(src, BuildMode::ReleaseFast);
    assert_eq!(outcome, RunOutcome::Ok, "OOB must not trap in ReleaseFast");
    assert_eq!(code, 0, "ReleaseFast OOB exits 0 (no bounds trap)");
    // Clamped to the last element (40); the value is defined and the program runs
    // to completion instead of panicking.
    assert_eq!(out, "done 40\n");
}

// =========================================================================
//  Regression tests for the v0.8 review findings.
// =========================================================================

#[test]
fn regression_switch_negative_label() {
    // A negative case label must match (it was silently dropped from the Switch).
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const x: i32 = -1;
    const r: u8 = switch (x) { -1 => 7, else => 0 };
    try out.print("{d}\n", .{r});
}
"#;
    assert_stdout(src, "7\n");
}

#[test]
fn regression_switch_negative_range() {
    // A negative inclusive range must route correctly.
    let src = r#"
fn sign(n: i32) []const u8 {
    return switch (n) {
        -100...-1 => "neg",
        0 => "zero",
        1...100 => "pos",
        else => "big",
    };
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{s} {s} {s}\n", .{ sign(-50), sign(0), sign(50) });
}
"#;
    assert_stdout(src, "neg zero pos\n");
}

#[test]
fn regression_truncate_wraps() {
    // @truncate narrows by wrapping: 300 & 0xFF == 44.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const x: u32 = 300;
    const y: u8 = @truncate(x);
    try out.print("{d}\n", .{y});
}
"#;
    assert_stdout(src, "44\n");
}

#[test]
fn regression_int_from_float_and_float_from_int() {
    // @intFromFloat truncates; @floatFromInt is lossless.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const f: f64 = 3.9;
    const n: i32 = @intFromFloat(f);
    const k: i32 = 7;
    const g: f64 = @floatFromInt(k);
    try out.print("{d} {d}\n", .{ n, g });
}
"#;
    assert_stdout(src, "3 7\n");
}

#[test]
fn regression_int_from_enum() {
    // @intFromEnum reads the variant's discriminant.
    let src = r#"
const Color = enum { red, green, blue };
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const c: Color = .green;
    const n: u8 = @intFromEnum(c);
    try out.print("{d}\n", .{n});
}
"#;
    assert_stdout(src, "1\n");
}

#[test]
fn regression_u128_high_bit_comparison() {
    // 2^127 (high bit set, stored as i128::MIN) must compare as a large unsigned.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var one: u128 = 1;
    const big = one << 127;
    const small: u128 = 5;
    try out.print("{} {}\n", .{ big > small, big < small });
}
"#;
    assert_stdout(src, "true false\n");
}

#[test]
fn regression_signed_negative_comparison_still_correct() {
    // Signed comparisons must remain correct after the cmp-repr change.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: i32 = -5;
    const b: i32 = 3;
    try out.print("{} {}\n", .{ a < b, a > b });
}
"#;
    assert_stdout(src, "true false\n");
}

#[test]
fn regression_slice_of_byvalue_array() {
    // A sub-slice of a by-value array carries the array's data (no NULL slice)
    // and starts at the low bound.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = [_]u8{ 1, 2, 3, 4 };
    const s = a[1..3];
    try out.print("{d},{d}\n", .{ s[0], s[1] });
}
"#;
    assert_stdout(src, "2,3\n");
}

#[test]
fn regression_for_over_slice_of_array() {
    // The reviewer's exact `for (arr[0..3])` repro sums to 60.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var arr = [_]u32{ 10, 20, 30, 40 };
    const s = arr[0..3];
    var sum: u32 = 0;
    for (s) |v| {
        sum = sum + v;
    }
    try out.print("sum of first 3 = {d}\n", .{sum});
}
"#;
    assert_stdout(src, "sum of first 3 = 60\n");
}

#[test]
fn regression_decimal_verb_on_float() {
    // `{d}` on a computed float renders the float, not `<int>`.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a: f64 = 7.0;
    const b: f64 = 2.0;
    try out.print("{d}\n", .{a / b});
}
"#;
    assert_stdout(src, "3.5\n");
}

#[test]
fn regression_radix_masks_negative_to_width() {
    // `{x}` on a negative signed int prints its two's-complement at its own width.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const c: i32 = -1;
    const b: i8 = -1;
    try out.print("{x} {x}\n", .{ c, b });
}
"#;
    assert_stdout(src, "ffffffff ff\n");
}

#[test]
fn regression_signed_min_div_minus_one_traps_in_debug() {
    // i32::MIN / -1 overflows the type; it must trap in Debug.
    let src = r#"
pub fn main(sys: *System) !void {
    var a: i32 = -2147483648;
    var b: i32 = -1;
    const c = a / b;
    _ = c;
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "MIN / -1 must trap");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("integer overflow")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn regression_signed_min_div_minus_one_wraps_in_release_fast() {
    // The same division wraps (yields MIN) with no trap in ReleaseFast.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var a: i32 = -2147483648;
    var b: i32 = -1;
    const c = a / b;
    try out.print("{d}\n", .{c});
}
"#;
    let (out, _err, outcome, code) = run_mode(src, BuildMode::ReleaseFast);
    assert_eq!(outcome, RunOutcome::Ok, "should not trap in ReleaseFast");
    assert_eq!(code, 0);
    assert_eq!(out, "-2147483648\n");
}

#[test]
fn regression_huge_alloc_is_a_clean_panic_not_an_abort() {
    // A multi-trillion-element allocation must exit nonzero with a clean
    // out-of-memory panic, never a Rust `handle_alloc_error` abort.
    let src = r#"
pub fn main(sys: *System) !void {
    const a = sys.heap;
    const s = try a.alloc(u8, 100000000000000);
    defer a.free(s);
    _ = s;
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "huge alloc must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("out of memory")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn regression_infinite_loop_terminates_with_budget_panic() {
    // A runaway loop must terminate with a clean budget panic and nonzero exit
    // (the wall-clock deadline bounds this to a few seconds in practice).
    let src = r#"
pub fn main(sys: *System) !void {
    var i: u64 = 0;
    while (true) {
        i = i + 1;
    }
    _ = i;
}
"#;
    let (_out, _err, outcome, code) = run(src);
    assert_ne!(code, 0, "infinite loop must exit nonzero");
    assert!(
        matches!(&outcome, RunOutcome::Panicked(m) if m.contains("instruction budget exhausted")),
        "outcome: {outcome:?}"
    );
}

#[test]
fn top_level_value_const_materializes_in_format_arg() {
    // v0.12 fix: a bare reference to a top-level (file-scope) value const has no
    // local slot, so it must inline its initializer instead of lowering to an
    // `undef` that the `{d}` formatter renders as the `<int>` placeholder.
    assert_stdout(
        "const N = 5;\npub fn main(sys: *System) !void { const o = sys.io.stdout(); \
         try o.print(\"{d}\\n\", .{N}); }\n",
        "5\n",
    );
}

#[test]
fn top_level_typed_const_materializes_in_format_arg() {
    // The same fix must cover a *sized*-int const (whose bound is `u32`, not
    // `comptime_int`, so the comptime-int fast path does not apply).
    assert_stdout(
        "const N: u32 = 42;\npub fn main(sys: *System) !void { const o = sys.io.stdout(); \
         try o.print(\"{d}\\n\", .{N}); }\n",
        "42\n",
    );
}

#[test]
fn regression_store_through_pointer_then_print_decimal() {
    // v0.15 finding 4: a `var x: i32` mutated through a `*i32` pointer, then
    // printed with `{d}`. An `address_taken` local's register holds a boxed
    // `Value::Ptr`, so a bare *value* read used to capture the raw pointer and the
    // formatter rendered `<int>` instead of the stored number. The bare read now
    // dereferences the box, so `{d}` prints the actual value (matching native).
    let src = r#"
fn bump(p: *i32) void {
    p.* = p.* + 1;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var x: i32 = 42;
    bump(&x);
    try out.print("x={d}\n", .{x});
}
"#;
    assert_stdout(src, "x=43\n");
}

#[test]
fn regression_address_taken_local_reads_current_value() {
    // The same boxing fix, without a store-through-pointer: simply taking a
    // local's address and then reading it by value must still render the number
    // (not the `<int>` placeholder).
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var x: i32 = 5;
    const p = &x;
    _ = p;
    try out.print("x={d}\n", .{x});
}
"#;
    assert_stdout(src, "x=5\n");
}

#[test]
fn regression_packed_struct_bitcast_to_integer() {
    // v0.21 (MAJOR): `@bitCast(packed struct) -> backing integer`. The VM stores
    // a packed struct as per-field values; it must pack them via the field bit
    // offsets/widths to match the native single-backing-integer image:
    // 5 | (20 << 3) | (200 << 8) == 51365. Previously the VM rendered `<int>`.
    let src = r#"
const P = packed struct { a: u3, b: u5, c: u8 };
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p: P = .{ .a = 5, .b = 20, .c = 200 };
    const raw: u16 = @bitCast(p);
    try out.print("{d}\n", .{raw});
}
"#;
    assert_stdout(src, "51365\n");
}

#[test]
fn regression_integer_bitcast_to_packed_struct_unpacks_fields() {
    // The reverse direction `@bitCast(int) -> packed struct` is VM-only (native
    // cleanly refuses a scalar-into-aggregate bitcast). It used to abort the VM
    // with an internal "field access on non-aggregate" error; it must now unpack
    // each field's bits, with sign-extension for a signed field:
    // 51365 -> { a=5, b=20, c=200 }; 0xFF over { a: i4, b: u4 } -> { a=-1, b=15 }.
    let src = r#"
const P = packed struct { a: u3, b: u5, c: u8 };
const S = packed struct { a: i4, b: u4 };
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const raw: u16 = 51365;
    const p: P = @bitCast(raw);
    const s: S = @bitCast(@as(u8, 0xFF));
    try out.print("{d} {d} {d} {d} {d}\n", .{ p.a, p.b, p.c, s.a, s.b });
}
"#;
    assert_stdout(src, "5 20 200 -1 15\n");
}

#[test]
fn regression_reduce_bool_vector_formats_as_bool() {
    // v0.21 (MAJOR): `@reduce(.And/.Or/.Xor, @Vector(N, bool))` declares a `bool`
    // result; the VM's bitwise fold used to yield an int, so `{}` printed `1`/`0`
    // instead of `true`/`false`. A bitwise op on two bools now carries a bool.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const t: @Vector(4, bool) = @splat(true);
    const f: @Vector(4, bool) = @splat(false);
    const x: @Vector(4, bool) = .{ true, false, true, true };
    try out.print("{} {} {}\n", .{ @reduce(.And, t), @reduce(.Or, f), @reduce(.Xor, x) });
}
"#;
    assert_stdout(src, "true false true\n");
}
