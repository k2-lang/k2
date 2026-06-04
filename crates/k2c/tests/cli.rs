//! End-to-end CLI tests for the `k2c` driver, exercising the `parse`
//! subcommand over real files and stdin. The binary path is provided by Cargo
//! via `CARGO_BIN_EXE_k2c`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Path to the built `k2c` binary.
fn k2c() -> Command {
    Command::new(env!("CARGO_BIN_EXE_k2c"))
}

/// Path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn parse_example_exits_zero_and_prints_tree() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().arg("parse").arg(&path).output().unwrap();
    assert!(out.status.success(), "expected success exit for hello.k2");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("(source-file"),
        "tree should start with `(source-file`, got: {}",
        &stdout[..stdout.len().min(40)]
    );
}

#[test]
fn parse_all_examples_exit_zero() {
    for entry in std::fs::read_dir(examples_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let out = k2c().arg("parse").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "`k2c parse {}` exited nonzero",
            path.display()
        );
    }
}

#[test]
fn parse_stdin_dash() {
    let mut child = k2c()
        .arg("parse")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"const x = 1;\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("(const x"), "got: {stdout}");
}

#[test]
fn parse_error_exits_nonzero() {
    // A syntax error must make `parse` exit nonzero (unlike `tokenize`).
    let mut child = k2c()
        .arg("parse")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"const x = ;\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "a parse error should yield a nonzero exit"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("error:"),
        "stderr should carry a diagnostic"
    );
}

#[test]
fn quiet_flag_suppresses_tree() {
    let mut child = k2c()
        .arg("parse")
        .arg("--quiet")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"const x = 1;\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.is_empty(),
        "--quiet must suppress the tree: {stdout:?}"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("item(s)"), "summary expected on stderr");
}

/// Runs `k2c` with `args`, feeding `input` on stdin, returning
/// `(success, stdout, stderr)`.
fn run_with_stdin(args: &[&str], input: &[u8]) -> (bool, String, String) {
    let mut child = k2c()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn fmt_stdin_prints_canonical_form() {
    let (ok, stdout, _) = run_with_stdin(&["fmt", "-"], b"const  x=1 ;\n");
    assert!(ok);
    assert_eq!(stdout, "const x = 1;\n");
}

#[test]
fn fmt_check_canonical_is_clean_and_silent() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().arg("fmt").arg("--check").arg(&path).output().unwrap();
    assert!(out.status.success(), "hello.k2 should already be canonical");
    assert!(out.stdout.is_empty(), "--check must not print to stdout");
}

#[test]
fn fmt_check_messy_input_exits_nonzero() {
    let (ok, stdout, stderr) = run_with_stdin(&["fmt", "--check", "-"], b"const  x=1 ;\n");
    assert!(!ok, "messy input must fail --check");
    assert!(stdout.is_empty(), "--check must not print to stdout");
    assert!(stderr.contains("not formatted"), "got: {stderr}");
}

#[test]
fn fmt_parse_error_exits_nonzero_no_stdout() {
    let (ok, stdout, stderr) = run_with_stdin(&["fmt", "-"], b"fn f( {\n");
    assert!(!ok, "a parse error must fail fmt");
    assert!(stdout.is_empty(), "no partial output on a parse error");
    assert!(stderr.contains("parse errors"), "got: {stderr}");
}

#[test]
fn fmt_write_stdin_is_rejected() {
    let out = k2c().arg("fmt").arg("--write").arg("-").output().unwrap();
    assert!(!out.status.success(), "--write of stdin must error");
}

#[test]
fn fmt_write_rewrites_then_is_check_clean() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("k2c_fmt_test_{}.k2", std::process::id()));
    std::fs::write(&path, b"const  x=1 ;\n").unwrap();

    let out = k2c().arg("fmt").arg("--write").arg(&path).output().unwrap();
    assert!(out.status.success(), "--write should succeed");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "const x = 1;\n");

    // Now it is canonical: --check is clean and a second --write is a no-op.
    let check = k2c().arg("fmt").arg("--check").arg(&path).output().unwrap();
    assert!(check.status.success(), "rewritten file must be canonical");
    let again = k2c().arg("fmt").arg("--write").arg(&path).output().unwrap();
    assert!(again.status.success());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn ast_matches_parse_tree() {
    let path = examples_dir().join("hello.k2");
    let ast = k2c().arg("ast").arg(&path).output().unwrap();
    let parse = k2c().arg("parse").arg(&path).output().unwrap();
    assert!(ast.status.success());
    assert_eq!(
        ast.stdout, parse.stdout,
        "`ast` and `parse` should print the same tree"
    );
}

#[test]
fn ast_stdin_dash() {
    let (ok, stdout, _) = run_with_stdin(&["ast", "-"], b"const x = 1;\n");
    assert!(ok);
    assert!(stdout.contains("(const x"), "got: {stdout}");
}

// ---- `resolve` subcommand --------------------------------------------------

#[test]
fn resolve_example_exits_zero_and_dumps_scopes() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().arg("resolve").arg(&path).output().unwrap();
    assert!(out.status.success(), "expected success exit for hello.k2");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("(scope #0"),
        "dump should start with `(scope #0`, got: {}",
        &stdout[..stdout.len().min(40)]
    );
}

#[test]
fn resolve_all_examples_exit_zero() {
    for entry in std::fs::read_dir(examples_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let out = k2c().arg("resolve").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "`k2c resolve {}` exited nonzero; stderr:\n{}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn resolve_uses_flag_dumps_uses_table() {
    let path = examples_dir().join("hello.k2");
    let out = k2c()
        .arg("resolve")
        .arg("--uses")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("(uses"),
        "expected a uses block, got: {stdout}"
    );
    assert!(stdout.contains("(modules"), "expected a modules block");
}

#[test]
fn resolve_undeclared_exits_nonzero() {
    let (ok, stdout, stderr) = run_with_stdin(&["resolve", "-"], b"fn f() i32 { return zzz; }\n");
    assert!(!ok, "an undeclared identifier must fail resolution");
    assert!(
        stderr.contains("use of undeclared identifier `zzz`"),
        "stderr should name the undeclared identifier, got: {stderr}"
    );
    assert!(
        stdout.is_empty(),
        "no scope dump should be printed on error, got: {stdout}"
    );
}

#[test]
fn resolve_parse_error_exits_nonzero_no_stdout() {
    let (ok, stdout, stderr) = run_with_stdin(&["resolve", "-"], b"fn f( {\n");
    assert!(!ok, "a parse error must gate resolution");
    assert!(stdout.is_empty(), "no dump on parse error, got: {stdout}");
    assert!(
        stderr.contains("it has parse errors"),
        "stderr should explain the parse-error gate, got: {stderr}"
    );
}

// ---- `check` subcommand ----------------------------------------------------

#[test]
fn check_example_exits_zero_and_prints_signatures() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().arg("check").arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "expected success exit for hello.k2; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("`main` : fn(*System) !void"),
        "expected a signature for main, got: {stdout}"
    );
}

#[test]
fn check_all_examples_exit_zero() {
    for entry in std::fs::read_dir(examples_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let out = k2c().arg("check").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "`k2c check {}` exited nonzero; stderr:\n{}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn check_uses_flag_dumps_type_table() {
    let path = examples_dir().join("hello.k2");
    let out = k2c()
        .arg("check")
        .arg("--uses")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("(types"),
        "expected a types block, got: {stdout}"
    );
}

#[test]
fn check_type_error_exits_nonzero() {
    let (ok, stdout, stderr) =
        run_with_stdin(&["check", "-"], b"fn f() void { const x: u8 = true; }\n");
    assert!(!ok, "a type error must make check exit nonzero");
    assert!(
        stderr.contains("error:") && stderr.contains("expected `u8`, found `bool`"),
        "stderr should carry the type diagnostic, got: {stderr}"
    );
    assert!(
        stdout.is_empty(),
        "no signature dump should be printed on error, got: {stdout}"
    );
}

#[test]
fn check_bad_assign_is_rejected() {
    // The milestone's sanity check: a deliberately wrong program is rejected.
    let (ok, _stdout, _stderr) = run_with_stdin(
        &["check", "-"],
        b"fn f() void { const x: u8 = true; _ = x; }\n",
    );
    assert!(!ok, "the bad assignment must be rejected");
}

#[test]
fn check_parse_error_gates_check() {
    let (ok, stdout, stderr) = run_with_stdin(&["check", "-"], b"fn f( {\n");
    assert!(!ok, "a parse error must gate type-checking");
    assert!(stdout.is_empty(), "no dump on parse error, got: {stdout}");
    assert!(
        stderr.contains("it has parse errors"),
        "stderr should explain the parse-error gate, got: {stderr}"
    );
}

#[test]
fn check_resolve_error_gates_check() {
    let (ok, stdout, stderr) = run_with_stdin(&["check", "-"], b"fn f() i32 { return zzz; }\n");
    assert!(!ok, "a resolution error must gate type-checking");
    assert!(stdout.is_empty(), "no dump on resolve error, got: {stdout}");
    assert!(
        stderr.contains("use of undeclared identifier `zzz`"),
        "stderr should carry the resolution diagnostic, got: {stderr}"
    );
}

// =========================================================================
//  `run` — compile to bytecode and execute `main` on the VM.
// =========================================================================

/// Runs `k2c` with `args` and `input` on stdin, returning
/// `(exit_code, stdout, stderr)`. Unlike [`run_with_stdin`], this surfaces the
/// numeric exit code so the safety-trap tests can assert it is nonzero.
fn run_with_code(args: &[&str], input: &[u8]) -> (i32, String, String) {
    let mut child = k2c()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn run_hello_exact_stdout_and_stderr_separation() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().arg("run").arg(&path).output().unwrap();
    assert!(out.status.success(), "hello.k2 should exit 0");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        stdout,
        "Hello, k2!\nk2 directs every joule of Sol: ~384600000000000000000000000 W.\n"
    );
    assert!(
        stderr.contains("(this line went to stderr)"),
        "the diagnostic line must go to stderr, got: {stderr}"
    );
    assert!(
        !stdout.contains("this line went to stderr"),
        "stderr content must NOT leak into stdout"
    );
}

#[test]
fn run_errors_example_exits_zero() {
    let path = examples_dir().join("errors.k2");
    let out = k2c().arg("run").arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "errors.k2 should run to completion, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("doubled(\"21\") = 42"), "got: {stdout}");
}

#[test]
fn run_bounds_trap_exits_nonzero_with_message() {
    let src = b"pub fn main(sys: *System) !void { const a = [_]u8{1,2,3}; const i: usize = 9; _ = a[i]; }";
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_ne!(code, 0, "an OOB index must exit nonzero");
    assert!(stdout.is_empty(), "no stdout on the trap, got: {stdout}");
    assert!(
        stderr.contains("panic:") && stderr.contains("index out of bounds"),
        "stderr should carry the panic message, got: {stderr}"
    );
}

#[test]
fn run_release_fast_skips_the_trap() {
    // The same overflowing add wraps instead of trapping under --release-fast.
    let src = b"pub fn main(sys: *System) !void { const out = sys.io.stdout(); var x: u8 = 250; const y: u8 = 10; x += y; try out.print(\"x = {d}\\n\", .{x}); }";
    let (code, stdout, _stderr) = run_with_code(&["run", "--release-fast", "-"], src);
    assert_eq!(code, 0, "ReleaseFast must not trap on the overflow");
    assert_eq!(stdout, "x = 4\n");
}

// =========================================================================
//  Standard library (v0.10): `@import("std")` as a REAL compiled module
// =========================================================================

/// The two std-heavy examples must run to completion with their exact expected
/// stdout and exit 0 — the milestone's hard acceptance gate.
#[test]
fn run_allocators_example_exact_output() {
    let path = examples_dir().join("allocators.k2");
    let out = k2c().arg("run").arg(&path).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        out.status.success(),
        "allocators.k2 must exit 0; stderr: {stderr}"
    );
    assert_eq!(
        stdout,
        "buf[15] = 225\n\
         list.len = 8, list.items[7] = 70\n\
         sum of first 8 squares = 140\n\
         arena handed out 32 + 64 bytes\n"
    );
}

#[test]
fn run_generic_list_example_exact_output() {
    let path = examples_dir().join("generic_list.k2");
    let out = k2c().arg("run").arg(&path).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        out.status.success(),
        "generic_list.k2 must exit 0; stderr: {stderr}"
    );
    assert_eq!(
        stdout,
        "nums.len = 3, sum = 142\n\
         nums[2] = 100, nums[99] = 0\n\
         words[0] = total\n\
         words[1] = control,\n\
         words[2] = zero waste\n"
    );
}

/// `std.ArrayList(T)` append/grow/len/deinit round-trip — a real monomorphized
/// container reached through the bundled std module.
#[test]
fn run_std_array_list_append_grow_deinit() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var list = std.ArrayList(u32).init(sys.heap);
    defer list.deinit();
    var i: u32 = 0;
    while (i < 20) : (i += 1) {
        try list.append(i * i);
    }
    try out.print("len={d} first={d} last={d}\n", .{ list.items.len, list.items[0], list.items[19] });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(
        code, 0,
        "ArrayList round-trip must exit 0; stderr: {stderr}"
    );
    assert_eq!(stdout, "len=20 first=0 last=361\n");
}

/// The handle-based `Allocator` floor: `alloc`/`free`/`realloc`/`create`/
/// `destroy` round-trips over the default allocator.
#[test]
fn run_allocator_method_round_trips() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const alloc = sys.heap;
    // alloc / free
    const s = try alloc.alloc(u32, 4);
    s[0] = 11;
    s[3] = 44;
    // realloc preserves contents
    const g = try alloc.realloc(s, 8);
    g[7] = 77;
    try out.print("g.len={d} g0={d} g3={d} g7={d}\n", .{ g.len, g[0], g[3], g[7] });
    alloc.free(g);
    // create / destroy
    const cell = try alloc.create(u32);
    cell.* = 99;
    try out.print("cell={d}\n", .{cell.*});
    alloc.destroy(cell);
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(
        code, 0,
        "allocator round-trips must exit 0; stderr: {stderr}"
    );
    assert_eq!(stdout, "g.len=8 g0=11 g3=44 g7=77\ncell=99\n");
}

/// The arena frees everything at once on `deinit`; no per-object frees needed.
#[test]
fn run_std_arena_allocator_bulk_free() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer {
        const leaked = gpa.deinit();
        if (leaked) @panic("leak");
    }
    var arena = std.heap.ArenaAllocator.init(gpa.allocator());
    defer arena.deinit();
    const a = arena.allocator();
    const x = try a.alloc(u8, 16);
    const y = try a.alloc(u8, 32);
    x[0] = 1;
    y[0] = 2;
    // No individual frees: the arena's deinit reclaims everything, and the GPA
    // must then report NO leak.
    try out.print("x.len={d} y.len={d}\n", .{ x.len, y.len });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "arena bulk-free must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "x.len=16 y.len=32\n");
}

/// The fixed-buffer allocator carves from a caller buffer and fails cleanly with
/// `OutOfMemory` once the buffer is exhausted.
#[test]
fn run_std_fixed_buffer_allocator() {
    let src = br#"
const std = @import("std");
/// Try to allocate `n` bytes; report whether it failed (the FBA returns a
/// CATCHABLE `error.OutOfMemory` once the backing buffer is exhausted, not a
/// panic). Frees on the success path.
fn tryAlloc(a: Allocator, n: usize) bool {
    const r = a.alloc(u8, n) catch {
        return true;
    };
    a.free(r);
    return false;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var storage = [_]u8{ 0, 0, 0, 0, 0, 0, 0, 0 };
    var fba = std.heap.FixedBufferAllocator.init(&storage);
    const a = fba.allocator();
    const x: []u8 = try a.alloc(u8, 4);
    defer a.free(x);
    x[0] = 7;
    try out.print("x.len={d} x0={d}\n", .{ x.len, x[0] });
    // 4 bytes fit (offset 0..4 of the 8-byte buffer); 1000 do not.
    const small_failed = tryAlloc(a, 4);
    const big_failed = tryAlloc(a, 1000);
    try out.print("small_failed={} big_failed={}\n", .{ small_failed, big_failed });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "FBA must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "x.len=4 x0=7\nsmall_failed=false big_failed=true\n");
}

/// The GPA reports NO leak when every allocation is freed (the correct program).
#[test]
fn run_gpa_no_leak_when_freed() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    const alloc = gpa.allocator();
    const buf = try alloc.alloc(u32, 8);
    alloc.free(buf);
    const leaked = gpa.deinit();
    if (leaked) @panic("unexpected leak");
    try out.print("no leak\n", .{});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "no-leak program must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "no leak\n");
}

/// The GPA detects a real leak (an allocation inside a loop the static pass
/// cannot see) at `deinit` and the program `@panic`s in Debug.
#[test]
fn run_gpa_detects_leak_and_panics_in_debug() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer {
        const leaked = gpa.deinit();
        if (leaked) @panic("memory leak detected at shutdown");
    }
    const alloc = gpa.allocator();
    var i: u32 = 0;
    while (i < 3) : (i += 1) {
        const buf = try alloc.alloc(u32, 4);
        buf[0] = i;
    }
    const out = sys.io.stdout();
    try out.print("allocated, never freed\n", .{});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_ne!(code, 0, "a leak must exit nonzero in Debug");
    assert!(
        stdout.contains("allocated, never freed"),
        "the body ran before shutdown; stdout: {stdout}"
    );
    assert!(
        stderr.contains("panic:"),
        "leak must panic; stderr: {stderr}"
    );
}

/// The GPA traps a double-free with a clean panic.
#[test]
fn run_gpa_detects_double_free() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer { _ = gpa.deinit(); }
    const alloc = gpa.allocator();
    const buf = try alloc.alloc(u32, 4);
    alloc.free(buf);
    alloc.free(buf);
}
"#;
    let (code, _stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_ne!(code, 0, "a double free must exit nonzero");
    assert!(
        stderr.contains("double free"),
        "stderr should name the double free; got: {stderr}"
    );
}

/// The GPA (and the managed heap) trap a use-after-free with a clean panic.
#[test]
fn run_gpa_detects_use_after_free() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer { _ = gpa.deinit(); }
    const alloc = gpa.allocator();
    const buf = try alloc.alloc(u32, 4);
    alloc.free(buf);
    const out = sys.io.stdout();
    try out.print("{d}\n", .{buf[0]});
}
"#;
    let (code, _stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_ne!(code, 0, "a use-after-free must exit nonzero");
    assert!(
        stderr.contains("use after free"),
        "stderr should name the UAF; got: {stderr}"
    );
}

/// `std.testing.expectEqual` / `expectError` are assertions-as-values: a passing
/// assertion is `void`, a failing one is an error the caller propagates.
#[test]
fn run_std_testing_helpers() {
    let src = br#"
const std = @import("std");
fn boom() !u32 {
    return error.Boom;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try std.testing.expectEqual(@as(u32, 7), 3 + 4);
    try std.testing.expectError(error.Boom, boom());
    // A failing expectEqual is a real error value, caught here.
    std.testing.expectEqual(@as(u32, 1), 2) catch {
        try out.print("caught mismatch\n", .{});
        return;
    };
    try out.print("unreachable\n", .{});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "testing helpers must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "caught mismatch\n");
}

// =========================================================================
//  v0.10 review fixes — regression tests for the verified defects.
// =========================================================================

/// FINDING #3: two FixedBufferAllocator sub-views must NOT alias. Carving `a`
/// and `b` from one backing array and writing `a[0]=11; b[0]=22` must read back
/// `11` and `22` (each sub-view is a disjoint window of the boxed array, honoured
/// via `ptr.offset` in `Heap::load_index`/`store_index`).
#[test]
fn run_fba_sub_views_do_not_alias() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    var storage = [_]u8{ 0, 0, 0, 0, 0, 0, 0, 0 };
    var fba = std.heap.FixedBufferAllocator.init(&storage);
    const al = fba.allocator();
    const a = try al.alloc(u8, 3);
    const b = try al.alloc(u8, 3);
    a[0] = 11;
    b[0] = 22;
    try o.print("a0={d} b0={d}\n", .{ a[0], b[0] });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "FBA sub-views must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "a0=11 b0=22\n");
}

/// FINDING #2: an FBA allocation with NO per-item free must compile clean (the
/// fixed-buffer allocator's free is a no-op by contract, so it is not a leak).
#[test]
fn run_fba_alloc_without_free_compiles_and_runs() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    var storage = [_]u8{ 0, 0, 0, 0, 0, 0, 0, 0 };
    var fba = std.heap.FixedBufferAllocator.init(&storage);
    const al = fba.allocator();
    const a = try al.alloc(u8, 4);
    a[0] = 9;
    try o.print("a0={d}\n", .{a[0]});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(
        code, 0,
        "unfreed FBA alloc must compile/run; stderr: {stderr}"
    );
    assert_eq!(stdout, "a0=9\n");
}

/// FINDING #2: an arena allocation with NO per-item free (only a bulk
/// `arena.deinit()`) must compile clean — and stays clean even without an
/// enclosing loop, which previously masked the false positive.
#[test]
fn run_arena_alloc_without_per_item_free_compiles() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    var base = std.heap.PageAllocator.init(sys);
    var arena = std.heap.ArenaAllocator.init(base.allocator());
    defer arena.deinit();
    const scratch = arena.allocator();
    const a = try scratch.alloc(u8, 32);
    a[0] = 7;
    try o.print("a0={d}\n", .{a[0]});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(
        code, 0,
        "unfreed arena alloc must compile/run; stderr: {stderr}"
    );
    assert_eq!(stdout, "a0=7\n");
}

/// FINDING #5: a FixedBufferAllocator over an `= undefined` backing buffer must
/// report the buffer's real capacity (16), not 1. Allocating 4 of 16 bytes must
/// succeed and round-trip element writes.
#[test]
fn run_fba_undefined_backing_buffer_capacity() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    var storage: [16]u8 = undefined;
    var fba = std.heap.FixedBufferAllocator.init(&storage);
    const al = fba.allocator();
    const a = try al.alloc(u8, 4);
    a[0] = 1;
    a[3] = 4;
    try o.print("len={d} a0={d} a3={d}\n", .{ a.len, a[0], a[3] });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "FBA undef-backing must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "len=4 a0=1 a3=4\n");
}

/// FINDING #6: `std.testing.allocator` (a namespaced runtime value const) must
/// resolve to a real, working GPA-tracked handle instead of panicking with
/// `unsupported intrinsic @std`. Alloc/free through it round-trips.
#[test]
fn run_std_testing_allocator_value_const() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    const a = std.testing.allocator;
    const buf = try a.alloc(u8, 4);
    buf[0] = 42;
    const v = buf[0];
    a.free(buf);
    try o.print("v={d}\n", .{v});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(
        code, 0,
        "std.testing.allocator must exit 0; stderr: {stderr}"
    );
    assert_eq!(stdout, "v=42\n");
}

/// FINDING #1/#4: a 16-element `[_]u8` array literal (and reading element 15)
/// must run with no host Rust panic — the register file is now sized to the
/// function's real operand-scratch demand, so a wide aggregate cannot overflow
/// the frame.
#[test]
fn run_sixteen_element_array_literal_no_panic() {
    let src = br#"
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    const a = [_]u8{ 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 };
    try o.print("{d}\n", .{a[15]});
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "16-elem array must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "15\n");
}

/// FINDING #1: a wide `print` (16 format args) must also run without overflowing
/// the register file.
#[test]
fn run_wide_print_sixteen_args_no_panic() {
    let src = br#"
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    try o.print("{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}{d}\n", .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "16-arg print must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "12345678910111213141516\n");
}

/// FINDING #4: an `= undefined` array supports element store/read, sub-slice, and
/// `.len` without an internal VM error.
#[test]
fn run_undefined_array_index_slice_len() {
    let src = br#"
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    var buf: [4]u8 = undefined;
    buf[0] = 9;
    const x = buf[0];
    const sl = buf[0..2];
    try o.print("x={d} sl={d} buf={d}\n", .{ x, sl.len, buf.len });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "undef-array use must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "x=9 sl=2 buf=4\n");
}

/// FINDING #7: the documented `*System` capability METHOD spellings must run
/// (not panic as `unsupported intrinsic`): clock now/monotonic/sleep, random
/// int/bytes, env get. Values are deterministic (clock from 0, fixed-seed PRNG,
/// env absent).
#[test]
fn run_clock_random_env_method_api() {
    let src = br#"
pub fn main(sys: *System) !void {
    const o = sys.io.stdout();
    const t0 = sys.clock.monotonicNanos();
    sys.clock.sleep(100);
    const t1 = sys.clock.now();
    const r = sys.random.int(u32);
    var rb: [2]u8 = undefined;
    sys.random.bytes(&rb);
    const e = sys.env.get("PATH");
    const absent = e == null;
    try o.print("t0={d} t1={d} rnz={} rb_set={} absent={}\n", .{ t0, t1, r != 0, rb[0] != rb[1] or rb[0] == rb[1], absent });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "capability methods must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "t0=0 t1=100 rnz=true rb_set=true absent=true\n");
}

/// hello.k2 and errors.k2 — the non-std examples — must still run unchanged now
/// that every `run` injects the bundled std.
#[test]
fn run_non_std_examples_still_work() {
    for name in ["hello.k2", "errors.k2"] {
        let path = examples_dir().join(name);
        let out = k2c().arg("run").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "{name} must still exit 0; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
