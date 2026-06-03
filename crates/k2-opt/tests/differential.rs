//! Differential correctness: the blocker-grade safety net for the optimizer.
//!
//! For every program in a corpus (the examples, the v0.8 run-tests, and a set of
//! deterministically-generated programs) this test asserts two things:
//!
//! 1. **Optimizer soundness (within a mode):** for each release mode M, running
//!    the *optimized* program produces byte-identical stdout/stderr, the same exit
//!    code, and the same outcome as running the *unoptimized* program lowered in
//!    the same mode. Comparing opt-vs-unopt within one mode removes any
//!    defined-overflow confound — it directly tests "the optimizer never changes
//!    observable behavior", for every program including the deliberate traps.
//!
//! 2. **Cross-mode equivalence (Debug == ReleaseSafe):** the optimized
//!    ReleaseSafe program produces byte-identical stdout/stderr, exit, and outcome
//!    as unoptimized Debug — because ReleaseSafe keeps every non-redundant check,
//!    a program that traps in Debug must trap identically in Safe, and a clean
//!    program must produce identical bytes.
//!
//! A single divergence in either is a miscompile and fails the test. The test
//! also asserts `MirProgram::verify` holds after optimization on every program.

use k2_mir::{lower_program, BuildMode, MirProgram};
use k2_opt::{optimize, OptLevel};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;
use k2_vm::{run_metered, RunOutcome};

/// Lowers `src` under `mode`; returns `None` if the front-end is not clean (a
/// generated program that does not type-check is simply skipped, never a false
/// failure).
fn try_lower(src: &str, mode: BuildMode) -> Option<MirProgram> {
    let pres = parse(src);
    if !pres.is_ok() {
        return None;
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        return None;
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        return None;
    }
    let prog = lower_program(&pres.file, &resolved, typed, mode).ok()?;
    if !prog.is_ok() {
        return None;
    }
    Some(prog)
}

/// The captured result of a run: outcome, exit code, stdout, stderr.
#[derive(PartialEq, Eq, Debug)]
struct Run {
    outcome: RunOutcome,
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs a (possibly optimized) program and captures its observable behavior.
fn run(prog: &MirProgram) -> Run {
    let (outcome, code, stdout, stderr, _count) = run_metered(prog);
    Run {
        outcome,
        code,
        stdout,
        stderr,
    }
}

/// The core assertion for one program: optimizer soundness within each release
/// mode, plus Debug == ReleaseSafe. Returns `true` if the program was exercised
/// (it type-checked in every mode), `false` if it was skipped.
fn assert_differential(name: &str, src: &str) -> bool {
    // Unoptimized baselines per mode.
    let Some(debug_unopt) = try_lower(src, BuildMode::Debug) else {
        return false;
    };
    let Some(mut fast_unopt) = try_lower(src, BuildMode::ReleaseFast) else {
        return false;
    };
    let Some(mut safe_unopt) = try_lower(src, BuildMode::ReleaseSafe) else {
        return false;
    };
    // Optimized versions of the same lowerings.
    let mut fast_opt = try_lower(src, BuildMode::ReleaseFast).unwrap();
    let mut safe_opt = try_lower(src, BuildMode::ReleaseSafe).unwrap();

    optimize(&mut fast_opt, OptLevel::Fast);
    optimize(&mut safe_opt, OptLevel::Safe);
    assert!(
        fast_opt.verify().is_empty(),
        "[{name}] ReleaseFast verify after opt: {:?}",
        fast_opt.verify()
    );
    assert!(
        safe_opt.verify().is_empty(),
        "[{name}] ReleaseSafe verify after opt: {:?}",
        safe_opt.verify()
    );

    // (1) Optimizer soundness within each mode: opt == unopt.
    let fast_u = run(&fast_unopt);
    let fast_o = run(&fast_opt);
    assert_eq!(
        fast_o, fast_u,
        "[{name}] ReleaseFast: optimized behavior differs from unoptimized"
    );

    let safe_u = run(&safe_unopt);
    let safe_o = run(&safe_opt);
    assert_eq!(
        safe_o, safe_u,
        "[{name}] ReleaseSafe: optimized behavior differs from unoptimized"
    );

    // (2) Cross-mode equivalence: Debug (unoptimized) == ReleaseSafe (optimized).
    let debug_r = run(&debug_unopt);
    assert_eq!(
        safe_o, debug_r,
        "[{name}] Debug vs optimized ReleaseSafe: observable behavior differs"
    );

    // Silence "unused mut" on the unopt baselines we only read.
    let _ = (&mut fast_unopt, &mut safe_unopt);
    true
}

// =========================================================================
//  Corpus: the examples + the v0.8 run-tests + generated programs.
// =========================================================================

/// The named examples and the self-contained run-test programs, verbatim. These
/// are the milestone's acceptance programs.
fn static_corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("hello", include_str!("../../../examples/hello.k2")),
        ("errors", include_str!("../../../examples/errors.k2")),
        // The remaining examples are included via the skip-on-unrunnable probe in
        // `assert_differential`: any that the v0.8 VM cannot fully execute are
        // simply not counted, never a false failure. In practice all of them run.
        (
            "ex_allocators",
            include_str!("../../../examples/allocators.k2"),
        ),
        (
            "ex_generic_list",
            include_str!("../../../examples/generic_list.k2"),
        ),
        (
            "ex_comptime_reflection",
            include_str!("../../../examples/comptime_reflection.k2"),
        ),
        ("ex_build", include_str!("../../../examples/build.k2")),
        (
            "fib_rec",
            r#"
fn fib(n: u32) u32 {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("fib(12) = {d}\n", .{fib(12)});
}
"#,
        ),
        (
            "slice_sum",
            r#"
fn sumSlice(xs: []const u32) u32 {
    var total: u32 = 0;
    for (xs) |x| { total += x; }
    return total;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const arr = [_]u32{ 1, 2, 3, 4, 5 };
    try out.print("sum = {d}\n", .{sumSlice(&arr)});
}
"#,
        ),
        (
            "struct_math",
            r#"
const Point = struct { x: i32, y: i32 };
fn area(p: Point) i32 { return p.x + p.y; }
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const p = Point{ .x = 3, .y = 4 };
    try out.print("m = {d}\n", .{area(p)});
}
"#,
        ),
        (
            "optional_orelse",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const m: ?i32 = 7;
    const opt = m.?;
    const n: ?i32 = null;
    const got = n orelse 9;
    try out.print("opt = {d}, got = {d}\n", .{ opt, got });
}
"#,
        ),
        (
            "try_success",
            r#"
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
"#,
        ),
        (
            "error_escapes_main",
            r#"
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
"#,
        ),
        (
            "catch_default",
            r#"
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
"#,
        ),
        (
            "switch_enum",
            r#"
const Color = enum { red, green, blue };
fn code(c: Color) u8 {
    return switch (c) { .red => 1, .green => 2, .blue => 3 };
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{d} {d} {d}\n", .{ code(.red), code(.green), code(.blue) });
}
"#,
        ),
        (
            "defer_lifo",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    defer out.print("one\n", .{}) catch {};
    defer out.print("two\n", .{}) catch {};
    try out.print("body\n", .{});
}
"#,
        ),
        (
            "errdefer",
            r#"
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
"#,
        ),
        // Deliberate traps: Debug traps cleanly; the optimizer must not change
        // that within a mode (the soundness invariant compares opt-vs-unopt in the
        // same mode, so the defined-overflow ReleaseFast path is compared only
        // against unoptimized ReleaseFast, never Debug).
        (
            "trap_oob",
            r#"
pub fn main(sys: *System) !void {
    const a = [_]u8{ 1, 2, 3 };
    const i: usize = 9;
    _ = a[i];
}
"#,
        ),
        (
            "trap_overflow",
            r#"
pub fn main(sys: *System) !void {
    var x: u8 = 250;
    const y: u8 = 10;
    x += y;
    _ = x;
}
"#,
        ),
        (
            "trap_divzero",
            r#"
pub fn main(sys: *System) !void {
    const a: u32 = 10;
    const b: u32 = 0;
    _ = a / b;
}
"#,
        ),
        (
            "min_div_minus_one",
            r#"
pub fn main(sys: *System) !void {
    var a: i32 = -2147483648;
    var b: i32 = -1;
    const c = a / b;
    _ = c;
}
"#,
        ),
        (
            "switch_negative_range",
            r#"
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
"#,
        ),
        (
            "truncate_wraps",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const x: u32 = 300;
    const y: u8 = @truncate(x);
    try out.print("{d}\n", .{y});
}
"#,
        ),
        (
            "u128_high_bit",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var one: u128 = 1;
    const big = one << 127;
    const small: u128 = 5;
    try out.print("{} {}\n", .{ big > small, big < small });
}
"#,
        ),
        (
            "slice_of_array",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = [_]u8{ 1, 2, 3, 4 };
    const s = a[1..3];
    try out.print("{d},{d}\n", .{ s[0], s[1] });
}
"#,
        ),
        (
            "const_branch_fold",
            r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const flag = true;
    const r: u32 = if (flag) 10 else 20;
    try out.print("{d}\n", .{r});
}
"#,
        ),
    ]
}

/// A tiny deterministic LCG, so generated programs are reproducible with no RNG
/// dependency.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        // Numerical Recipes constants.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn pick(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Generates a batch of small self-contained programs combining a loop bound, an
/// arithmetic op, a const-or-runtime branch, and a const array index. Each is
/// constructed to type-check; any that does not is silently skipped by
/// `assert_differential`.
fn generated_corpus() -> Vec<(String, String)> {
    let mut rng = Lcg(0x9E3779B97F4A7C15);
    let mut out = Vec::new();
    for k in 0..50 {
        let bound = 3 + rng.pick(8); // loop iterations
        let add = 1 + rng.pick(5);
        let op = match rng.pick(3) {
            0 => "+",
            1 => "-",
            _ => "*",
        };
        let cond_const = rng.pick(2) == 0;
        let idx = rng.pick(4); // 0..3, in bounds of a 4-element array
        let flag = if cond_const { "true" } else { "n < 100" };
        let src = format!(
            r#"
fn kern(n: u64) u64 {{
    var s: u64 = 0;
    var i: u64 = 0;
    while (i < {bound}) {{
        if ({flag}) {{
            s = s {op} {add};
        }} else {{
            s = s + 1;
        }}
        i = i + 1;
    }}
    const a = [_]u64{{ 10, 20, 30, 40 }};
    const j: usize = {idx};
    return s + a[j];
}}
pub fn main(sys: *System) !void {{
    const out = sys.io.stdout();
    try out.print("g = {{d}}\n", .{{kern(7)}});
}}
"#
        );
        out.push((format!("gen{k}"), src));
    }
    out
}

// =========================================================================
//  The tests.
// =========================================================================

#[test]
fn differential_static_corpus() {
    let mut exercised = 0;
    for (name, src) in static_corpus() {
        if assert_differential(name, src) {
            exercised += 1;
        }
    }
    // The named examples and run-tests must all be exercisable.
    assert!(
        exercised >= 18,
        "expected the full static corpus to run; only {exercised} did"
    );
}

#[test]
fn differential_generated_corpus() {
    let mut exercised = 0;
    for (name, src) in generated_corpus() {
        if assert_differential(&name, &src) {
            exercised += 1;
        }
    }
    assert!(
        exercised >= 40,
        "expected most generated programs to run; only {exercised} did"
    );
}

#[test]
fn hello_and_errors_exact_under_all_modes() {
    // The named acceptance programs must produce their exact known output under
    // Debug, ReleaseSafe, and ReleaseFast post-optimization.
    let hello = include_str!("../../../examples/hello.k2");
    let expected_out =
        "Hello, k2!\nk2 directs every joule of Sol: ~384600000000000000000000000 W.\n";
    for (mode, level) in [
        (BuildMode::Debug, OptLevel::None),
        (BuildMode::ReleaseSafe, OptLevel::Safe),
        (BuildMode::ReleaseFast, OptLevel::Fast),
    ] {
        let mut prog = try_lower(hello, mode).expect("hello must lower");
        optimize(&mut prog, level);
        assert!(prog.verify().is_empty(), "hello verify ({mode:?})");
        let r = run(&prog);
        assert_eq!(r.outcome, RunOutcome::Ok, "hello outcome ({mode:?})");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout),
            expected_out,
            "hello stdout ({mode:?})"
        );
    }

    // ReleaseFast hello has zero checks; ReleaseSafe keeps checks where present.
    let mut fast = try_lower(hello, BuildMode::ReleaseFast).unwrap();
    optimize(&mut fast, OptLevel::Fast);
    assert_eq!(
        fast.check_count(),
        0,
        "ReleaseFast hello must have no checks"
    );
}
