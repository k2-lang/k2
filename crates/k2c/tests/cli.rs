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

// =========================================================================
//  v0.12 — `k2c build`, multi-file modules, build_options, and the lockfile
// =========================================================================

/// Runs `k2c build [args]` from the `examples/` directory (the build root) and
/// returns the completed output. A temp `build.lock` is written there.
fn build_in_examples(args: &[&str]) -> std::process::Output {
    let mut cmd = k2c();
    cmd.arg("build");
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(examples_dir()).output().unwrap()
}

#[test]
fn build_describes_the_dag() {
    let out = build_in_examples(&[]);
    assert!(
        out.status.success(),
        "`k2c build` must describe the DAG and exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The library, the five executables, and the run/test steps appear.
    assert!(
        stdout.contains("[lib] examples_support"),
        "missing lib in: {stdout}"
    );
    assert!(stdout.contains("[exe] hello"), "missing hello exe");
    assert!(stdout.contains("run :"), "missing run step");
    assert!(stdout.contains("test :"), "missing test step");
}

#[test]
fn build_run_hello_prints_greeting() {
    let out = build_in_examples(&["run", "-Dexample=hello"]);
    assert!(
        out.status.success(),
        "`build run -Dexample=hello` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l == "Hello, k2!"),
        "expected `Hello, k2!` in: {stdout}"
    );
}

#[test]
fn build_run_selects_example_by_option() {
    let out = build_in_examples(&["run", "-Dexample=allocators"]);
    assert!(
        out.status.success(),
        "`build run -Dexample=allocators` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The allocators example prints a `list.len = ...` line; hello does not.
    assert!(
        stdout.contains("list.len"),
        "expected allocators output, got: {stdout}"
    );
}

#[test]
fn build_test_runs_the_test_blocks() {
    let out = build_in_examples(&["test"]);
    assert!(
        out.status.success(),
        "`k2c build test` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The per-test report + summary go to stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("0 failed"),
        "expected all tests to pass: {stderr}"
    );
}

/// Creates an isolated temp project directory with a minimal `build.k2` + a
/// `main.k2` that reads `@import("build_options")`, returning the directory. Each
/// caller uses a unique subdir so the lock-file tests never race on a shared
/// file. The directory is left in place (it lives under the OS temp dir).
fn temp_build_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("k2_build_test_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("build.k2"),
        r#"const build = @import("build");
pub fn build(b: *Build) void {
    const target = b.standardTarget();
    const optimize = b.standardOptimize();
    const verbose = b.option(bool, "verbose", "verbose") orelse false;
    const exe = b.addExecutable(.{
        .name = "main",
        .root_source = b.path("main.k2"),
        .target = target,
        .optimize = optimize,
    });
    exe.addOption(bool, "verbose", verbose);
    b.installArtifact(exe);
    const run_step = b.step("run", "run it");
    const run_exe = b.addRunArtifact(exe);
    run_step.dependOn(&run_exe.step);
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.k2"),
        r#"const std = @import("std");
const opts = @import("build_options");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    if (opts.verbose) {
        try out.print("verbose\n", .{});
    } else {
        try out.print("quiet\n", .{});
    }
}
"#,
    )
    .unwrap();
    dir
}

#[test]
fn build_options_drive_a_comptime_branch() {
    let dir = temp_build_project("opts");
    // Default (verbose=false) -> "quiet".
    let out = k2c()
        .arg("build")
        .arg("run")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "default run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("quiet"),
        "expected quiet branch"
    );
    // -Dverbose=true -> "verbose" (the comptime-known branch flips).
    let out = k2c()
        .arg("build")
        .arg("run")
        .arg("-Dverbose=true")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "verbose run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("verbose"),
        "expected verbose branch"
    );
}

#[test]
fn lockfile_is_reproducible() {
    let dir = temp_build_project("lock");
    let run = || k2c().arg("build").current_dir(&dir).output().unwrap();
    assert!(run().status.success());
    let lock1 = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    assert!(run().status.success());
    let lock2 = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    assert_eq!(
        lock1, lock2,
        "the lock must be reproducible for identical inputs"
    );
    assert!(
        lock1.starts_with("# k2 build lock v1"),
        "lock header missing"
    );
    // The recorded option surfaces in the lock.
    assert!(
        lock1.contains("exe main"),
        "lock should list the exe: {lock1}"
    );
}

#[test]
fn multi_file_path_import_runs() {
    // A root program that path-imports a sibling library compiles + runs as one
    // merged module graph through `k2c run`.
    let dir = std::env::temp_dir().join(format!("k2_multi_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(
        dir.join("lib/util.k2"),
        "pub fn triple(x: u32) u32 { return x * 3; }\npub const NAME: u32 = 7;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("app.k2"),
        r#"const std = @import("std");
const util = @import("./lib/util.k2");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("triple={d} name={d}\n", .{ util.triple(4), util.NAME });
}
"#,
    )
    .unwrap();
    let out = k2c().arg("run").arg(dir.join("app.k2")).output().unwrap();
    assert!(
        out.status.success(),
        "multi-file run must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("triple=12 name=7"),
        "expected `triple=12 name=7`, got: {stdout}"
    );
}

// ===========================================================================
//  v0.12 regression tests — the ten review findings.
// ===========================================================================

/// Runs `k2c run -` (stdin) on `source` and returns `(success, stdout, stderr)`.
fn run_stdin(source: &str) -> (bool, String, String) {
    let mut child = k2c()
        .arg("run")
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
        .write_all(source.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs `k2c run <path>` and returns `(success, stdout, stderr)`.
fn run_file(path: &std::path::Path) -> (bool, String, String) {
    let out = k2c().arg("run").arg(path).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn common_top_level_const_names_compile_and_run() {
    // Finding (CRITICAL): a user top-level binding whose name coincides with an
    // identifier std uses as a function parameter/local (`eql`, `a`, `b`, `T`,
    // `x`, `value`, `n`) must NOT be rejected as an illegal shadow, and must
    // print its value (not the `<int>` undef placeholder).
    for (name, val) in [
        ("eql", "5"),
        ("a", "7"),
        ("b", "9"),
        ("T", "1"),
        ("x", "3"),
        ("value", "8"),
        ("n", "4"),
    ] {
        let src = format!(
            "const {name} = {val}; pub fn main(sys: *System) !void {{ \
             const o = sys.io.stdout(); try o.print(\"{{d}}\\n\", .{{{name}}}); }}"
        );
        let (ok, stdout, stderr) = run_stdin(&src);
        assert!(
            ok,
            "`const {name} = {val}` must compile + run; stderr: {stderr}"
        );
        assert_eq!(
            stdout.trim_end(),
            val,
            "`const {name} = {val}` must print {val}, got: {stdout:?}"
        );
    }
}

#[test]
fn typed_and_string_top_level_consts_materialize() {
    // The same fix must materialize a *typed* int const and a string const, not
    // just untyped comptime-int ones.
    let (ok, stdout, stderr) = run_stdin(
        "const a: u32 = 7; pub fn main(sys: *System) !void { \
         const o = sys.io.stdout(); try o.print(\"{d}\\n\", .{a}); }",
    );
    assert!(ok, "typed const must run; stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "7");

    let (ok, stdout, stderr) = run_stdin(
        "const name: []const u8 = \"hi\"; pub fn main(sys: *System) !void { \
         const o = sys.io.stdout(); try o.print(\"{s}\\n\", .{name}); }",
    );
    assert!(ok, "string const must run; stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "hi");
}

#[test]
fn stdin_path_import_is_a_clean_compile_error() {
    // Finding (MINOR): `k2c run -` with a relative path import must produce a
    // CLEAN compile-time diagnostic, never a deferred runtime VM panic.
    let (ok, _stdout, stderr) =
        run_stdin("const x = @import(\"./nope.k2\"); pub fn main(sys: *System) !void {}");
    assert!(!ok, "a stdin path import must not succeed");
    assert!(
        stderr.contains("cannot resolve") && stderr.contains("from stdin"),
        "expected a clear stdin-path-import diagnostic, got: {stderr}"
    );
    assert!(
        !stderr.contains("unsupported intrinsic") && !stderr.contains("panic:"),
        "the failure must be a compile-time error, not a runtime panic: {stderr}"
    );
}

#[test]
fn self_import_through_root_resolves() {
    // Finding (BLOCKER/MAJOR): a `@import` that resolves back to the root file
    // (a self-import) must resolve to the root's namespace, per spec cycles.
    let dir = std::env::temp_dir().join(format!("k2_selfimport_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d.k2"),
        "const me = @import(\"./d.k2\");\npub const V: u32 = 7;\n\
         pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
         try o.print(\"self {d}\\n\", .{me.V}); }\n",
    )
    .unwrap();
    let (ok, stdout, stderr) = run_file(&dir.join("d.k2"));
    assert!(ok, "self-import must resolve + run; stderr: {stderr}");
    assert!(
        stdout.contains("self 7"),
        "expected `self 7`, got: {stdout}"
    );
}

#[test]
fn import_cycle_through_root_resolves() {
    // Finding (BLOCKER/MAJOR): an A<->B cycle where A is the root must resolve.
    let dir = std::env::temp_dir().join(format!("k2_cycle_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.k2"),
        "const b = @import(\"./b.k2\");\npub const AV: u32 = 10;\n\
         pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
         try o.print(\"cycle {d}\\n\", .{b.BV}); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.k2"),
        "const a = @import(\"./a.k2\");\npub const BV: u32 = 20;\n\
         pub fn helper() u32 { return a.AV; }\n",
    )
    .unwrap();
    let (ok, stdout, stderr) = run_file(&dir.join("a.k2"));
    assert!(
        ok,
        "cycle through root must resolve + run; stderr: {stderr}"
    );
    assert!(
        stdout.contains("cycle 20"),
        "expected `cycle 20`, got: {stdout}"
    );
}

#[test]
fn same_basename_files_outside_root_do_not_collide() {
    // Finding (MAJOR): two distinct files that share a basename, reached via
    // `../`, must NOT collide on the same synthetic namespace.
    let base = std::env::temp_dir().join(format!("k2_basename_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("proj")).unwrap();
    std::fs::create_dir_all(base.join("other")).unwrap();
    std::fs::write(base.join("shared.k2"), "pub const V: u32 = 11;\n").unwrap();
    std::fs::write(base.join("other/shared.k2"), "pub const V: u32 = 22;\n").unwrap();
    std::fs::write(
        base.join("proj/main2.k2"),
        "const s1 = @import(\"../shared.k2\");\n\
         const s2 = @import(\"../other/shared.k2\");\n\
         pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
         try o.print(\"{d} {d}\\n\", .{ s1.V, s2.V }); }\n",
    )
    .unwrap();
    let (ok, stdout, stderr) = run_file(&base.join("proj/main2.k2"));
    assert!(ok, "same-basename files must not collide; stderr: {stderr}");
    assert!(stdout.contains("11 22"), "expected `11 22`, got: {stdout}");
}

/// Writes a `build.k2` + sources into a fresh temp dir keyed on `tag`, returning
/// the directory. The `build_body` is the body of `pub fn build(b: *Build)`.
fn temp_build(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("k2_bt_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }
    dir
}

#[test]
fn build_step_runs_all_reachable_run_artifacts_in_declaration_order() {
    // Finding (BLOCKER): a user step depending on TWO run-exes must run BOTH, in
    // declaration order (first-declared first).
    let dir = temp_build(
        "multirun",
        &[
            (
                "build.k2",
                r#"const build = @import("build");
pub fn build(b: *Build) void {
    const t = b.standardTarget();
    const o = b.standardOptimize();
    const e1 = b.addExecutable(.{ .name = "e1", .root_source = b.path("e1.k2"), .target = t, .optimize = o });
    const e2 = b.addExecutable(.{ .name = "e2", .root_source = b.path("e2.k2"), .target = t, .optimize = o });
    const r1 = b.addRunArtifact(e1);
    const r2 = b.addRunArtifact(e2);
    const all = b.step("all", "run both");
    all.dependOn(&r1.step);
    all.dependOn(&r2.step);
}
"#,
            ),
            (
                "e1.k2",
                "pub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"EX1\\n\", .{}); }\n",
            ),
            (
                "e2.k2",
                "pub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"EX2\\n\", .{}); }\n",
            ),
        ],
    );
    let out = k2c()
        .arg("build")
        .arg("all")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build all must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("EX1"), "EX1 must run, got: {stdout}");
    assert!(stdout.contains("EX2"), "EX2 must run, got: {stdout}");
    // Declaration order: EX1 before EX2.
    let p1 = stdout.find("EX1").unwrap();
    let p2 = stdout.find("EX2").unwrap();
    assert!(
        p1 < p2,
        "EX1 must run before EX2 (declaration order): {stdout}"
    );
}

#[test]
fn build_test_step_runs_all_reachable_test_suites() {
    // Finding (BLOCKER): a `test` step depending on TWO test artifacts must run
    // BOTH suites and aggregate the reports (sum passed/failed).
    let dir = temp_build(
        "multitest",
        &[
            (
                "build.k2",
                r#"const build = @import("build");
pub fn build(b: *Build) void {
    const t = b.standardTarget();
    const o = b.standardOptimize();
    const ta = b.addTest(.{ .name = "ta", .root_source = b.path("a.k2"), .target = t, .optimize = o });
    const tb = b.addTest(.{ .name = "tb", .root_source = b.path("b.k2"), .target = t, .optimize = o });
    const ra = b.addRunArtifact(ta);
    const rb = b.addRunArtifact(tb);
    const test_step = b.step("test", "run both suites");
    test_step.dependOn(&ra.step);
    test_step.dependOn(&rb.step);
}
"#,
            ),
            (
                "a.k2",
                "const std = @import(\"std\");\ntest \"in A\" { try std.testing.expect(true); }\n",
            ),
            (
                "b.k2",
                "const std = @import(\"std\");\ntest \"in B\" { try std.testing.expect(true); }\n",
            ),
        ],
    );
    let out = k2c()
        .arg("build")
        .arg("test")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build test must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Both suites ran (each prints its test name) and the aggregate is 2 passed.
    assert!(stderr.contains("in A"), "suite A must run: {stderr}");
    assert!(stderr.contains("in B"), "suite B must run: {stderr}");
    assert!(
        stderr.contains("2 passed"),
        "aggregate must be 2 passed: {stderr}"
    );
}

/// Builds + runs a project that reads `@import("build_options")` for an option of
/// the given declared type, with the given `-D` flag, returning the program's
/// stdout (or stderr on failure).
fn build_option_project(
    tag: &str,
    decl_type: &str,
    body: &str,
    dflag: Option<&str>,
) -> (bool, String, String) {
    let build = format!(
        r#"const build = @import("build");
pub fn build(b: *Build) void {{
    const t = b.standardTarget();
    const o = b.standardOptimize();
    const v = b.option({decl_type}, "opt", "an option") orelse {default};
    const exe = b.addExecutable(.{{ .name = "main", .root_source = b.path("main.k2"), .target = t, .optimize = o }});
    exe.addOption({decl_type}, "opt", v);
    b.installArtifact(exe);
    const run_step = b.step("run", "run it");
    const run_exe = b.addRunArtifact(exe);
    run_step.dependOn(&run_exe.step);
}}
"#,
        default = match decl_type {
            "bool" => "false",
            "[]const u8" => "\"none\"",
            _ => "0",
        }
    );
    let dir = temp_build(tag, &[("build.k2", &build), ("main.k2", body)]);
    let mut cmd = k2c();
    cmd.arg("build").arg("run");
    if let Some(d) = dflag {
        cmd.arg(d);
    }
    let out = cmd.current_dir(&dir).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn bool_build_option_honors_its_declared_type() {
    // Finding (BLOCKER/MAJOR): a `bool` option must accept 1/0/true/false/yes/no
    // and never break the build for a non-true/false value.
    let body = "const opts = @import(\"build_options\");\n\
        pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
        if (opts.opt) { try o.print(\"on\\n\", .{}); } else { try o.print(\"off\\n\", .{}); } }\n";
    for (flag, expect) in [
        (Some("-Dopt=true"), "on"),
        (Some("-Dopt=1"), "on"),
        (Some("-Dopt=yes"), "on"),
        (Some("-Dopt=false"), "off"),
        (Some("-Dopt=0"), "off"),
        (Some("-Dopt=notabool"), "off"),
        (None, "off"),
    ] {
        let (ok, stdout, stderr) = build_option_project("optbool", "bool", body, flag);
        assert!(
            ok,
            "bool option {flag:?} must not break the build; stderr: {stderr}"
        );
        assert_eq!(
            stdout.trim_end(),
            expect,
            "bool option {flag:?} expected {expect}"
        );
    }
}

#[test]
fn string_build_option_keeps_numeric_values_as_strings() {
    // Finding (BLOCKER): a `[]const u8` option given a numeric value must stay a
    // string (no `.len`-on-an-int build-script panic).
    let body = "const opts = @import(\"build_options\");\n\
        pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
        try o.print(\"{s}\\n\", .{opts.opt}); }\n";
    let (ok, stdout, stderr) =
        build_option_project("optstr", "[]const u8", body, Some("-Dopt=123"));
    assert!(
        ok,
        "string option with numeric value must not panic; stderr: {stderr}"
    );
    assert_eq!(stdout.trim_end(), "123");
}

#[test]
fn int_build_option_honors_its_declared_type() {
    // Finding (BLOCKER): an `i64` option parses its value as an integer.
    let body = "const opts = @import(\"build_options\");\n\
        pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
        try o.print(\"{d}\\n\", .{opts.opt}); }\n";
    let (ok, stdout, stderr) = build_option_project("optint", "i64", body, Some("-Dopt=42"));
    assert!(ok, "int option must build; stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "42");
}

#[test]
fn lockfile_fingerprints_artifact_sources_and_flips_on_change() {
    // Finding (BLOCKER): editing an artifact source (not just build.k2) must flip
    // graph_hash; restoring it must restore the hash; identical inputs are
    // byte-reproducible.
    let dir = temp_build(
        "locksrc",
        &[
            (
                "build.k2",
                r#"const build = @import("build");
pub fn build(b: *Build) void {
    const t = b.standardTarget();
    const o = b.standardOptimize();
    const exe = b.addExecutable(.{ .name = "main", .root_source = b.path("main.k2"), .target = t, .optimize = o });
    b.installArtifact(exe);
}
"#,
            ),
            ("main.k2", "pub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"v1\\n\", .{}); }\n"),
        ],
    );
    let build = || k2c().arg("build").current_dir(&dir).output().unwrap();
    assert!(build().status.success());
    let lock1 = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    // The lock must list the artifact source, not just build.k2.
    assert!(
        lock1.contains("main.k2 h="),
        "lock must fingerprint main.k2: {lock1}"
    );
    let hash1 = lock1
        .lines()
        .find(|l| l.starts_with("graph_hash"))
        .unwrap()
        .to_string();

    // Reproducible: a second build over identical inputs is byte-identical.
    assert!(build().status.success());
    let lock1b = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    assert_eq!(
        lock1, lock1b,
        "identical inputs must produce a byte-identical lock"
    );

    // Editing the artifact source flips graph_hash.
    std::fs::write(
        dir.join("main.k2"),
        "pub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"v2\\n\", .{}); }\n",
    )
    .unwrap();
    assert!(build().status.success());
    let lock2 = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    let hash2 = lock2
        .lines()
        .find(|l| l.starts_with("graph_hash"))
        .unwrap()
        .to_string();
    assert_ne!(
        hash1, hash2,
        "editing an artifact source must flip graph_hash"
    );

    // Restoring the source restores the original hash.
    std::fs::write(
        dir.join("main.k2"),
        "pub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"v1\\n\", .{}); }\n",
    )
    .unwrap();
    assert!(build().status.success());
    let lock3 = std::fs::read_to_string(dir.join("build.lock")).unwrap();
    let hash3 = lock3
        .lines()
        .find(|l| l.starts_with("graph_hash"))
        .unwrap()
        .to_string();
    assert_eq!(hash1, hash3, "restoring the source must restore graph_hash");
}
