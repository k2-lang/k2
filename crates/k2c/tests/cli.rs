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
    // The diagnostic is rendered in the rich caret format: a header, a `-->`
    // locator, the source line, and a `^` underline beneath the offending token.
    assert!(
        stderr.contains("-->"),
        "rich locator missing, got: {stderr}"
    );
    assert!(
        stderr.contains("const x: u8 = true;"),
        "source line missing, got: {stderr}"
    );
    let lines: Vec<&str> = stderr.lines().collect();
    let src_idx = lines
        .iter()
        .position(|l| l.contains("const x: u8 = true;"))
        .expect("source line present");
    let underline = lines[src_idx + 1];
    let caret = underline.find('^').expect("caret row follows the source");
    let bar = underline.find('|').expect("gutter bar on caret row");
    // The caret column (relative to the source text start) must land under
    // `true` — the offending value.
    let src_line = lines[src_idx];
    let src_bar = src_line.find('|').unwrap();
    let true_col = src_line.find("true").unwrap() - (src_bar + 2);
    assert_eq!(
        caret - (bar + 2),
        true_col,
        "caret must sit under `true`\n{stderr}"
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
//  Native backend: signal-killed child vs real panic-trap exit code
//  (Finding 6). Gated to x86_64 Linux, where `run-native` executes the ELF.
// =========================================================================

/// A genuine k2 panic-trap under `run-native` exits 134 (the binary prints a
/// `panic:` line then `exit(134)`), while a signal-killed child (a SIGSEGV from
/// native stack exhaustion on deep recursion) is reported as `128 + signo`
/// (139 for SIGSEGV) — distinguishable from the trap, not conflated with it.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn run_native_distinguishes_signal_from_trap() {
    // (a) A real overflow trap: exit 134, with the `panic:` line on stderr.
    let trap = b"pub fn main(sys: *System) u8 { var x: u8 = 255; x = x + 1; return x; }";
    let (code, _out, stderr) = run_with_code(&["run-native", "-"], trap);
    assert_eq!(code, 134, "a real trap exits 134 via run-native");
    assert!(
        stderr.contains("panic:"),
        "the trap prints a panic line, got: {stderr}"
    );

    // (b) Deep recursion exhausts the native process stack -> SIGSEGV. The driver
    // reports 128 + SIGSEGV (11) = 139, NOT 134, so the crash is not silently
    // reported as an ordinary k2 trap. No `panic:` line is printed (a raw fault).
    let segv = b"fn sum(n: i64) i64 { if (n == 0) { return 0; } return n + sum(n - 1); } \
          pub fn main(sys: *System) u8 { var r: i64 = sum(50000000); return 7; }";
    let (code, _out, stderr) = run_with_code(&["run-native", "-"], segv);
    assert_eq!(
        code, 139,
        "a SIGSEGV-killed child reports 128+signo (139), not the trap's 134"
    );
    assert!(
        !stderr.contains("panic:"),
        "a raw signal fault prints no k2 panic line, got: {stderr}"
    );
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

/// The v0.22 `data_structures.k2` example (HashMap + sort + unicode + math/
/// bignum) runs to completion with its exact expected stdout and exit 0.
#[test]
fn run_data_structures_example_exact_output() {
    let path = examples_dir().join("data_structures.k2");
    let out = k2c().arg("run").arg(&path).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        out.status.success(),
        "data_structures.k2 must exit 0; stderr: {stderr}"
    );
    assert_eq!(
        stdout,
        "map: count=170 removed=86 has9=0 has10=1 get10=100 live_sum=3684665\n\
         sort: asc_min=-100 asc_max=99 isSorted=1 found13=1 desc_first=99\n\
         unicode: cafe_len=4 euro_cp=8364 euro_bytes=3 lone_valid=0\n\
         math: gcd=21 pow=65536 clamp=255 bigmul=1219326311336229232209\n"
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

// =========================================================================
//  Standard library (v0.22): HashMap, sort, unicode, math/bignum, allocators
// =========================================================================

/// HEADLINE ACCEPTANCE: a `std.IntHashMap(u32, u64)` takes 1000 inserts (forcing
/// several grows past the 75% load factor), reads every key back, updates one,
/// removes all even keys (tombstones), reinserts them (reusing tombstones), and
/// iterates the live set — every result exact, leak-clean, exit 0.
#[test]
fn run_std_hashmap_stress_resize_remove_iterate() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer { const leaked = gpa.deinit(); if (leaked) @panic("leak"); }
    const alloc = gpa.allocator();
    var map = std.IntHashMap(u32, u64).init(alloc);
    defer map.deinit();
    var i: u32 = 0;
    while (i < 1000) : (i += 1) try map.put(i, @as(u64, i) * 11);
    var readback: usize = 0;
    i = 0;
    while (i < 1000) : (i += 1) {
        if (map.get(i)) |v| { if (v == @as(u64, i) * 11) readback += 1; }
    }
    try map.put(7, 7777);
    var removed: usize = 0;
    i = 0;
    while (i < 1000) : (i += 1) {
        if (i % 2 == 0) { if (map.remove(i)) removed += 1; }
    }
    const has4: u32 = if (map.contains(4)) 1 else 0;
    const has7: u32 = if (map.contains(7)) 1 else 0;
    i = 0;
    while (i < 1000) : (i += 1) {
        if (i % 2 == 0) try map.put(i, @as(u64, i) * 11);
    }
    var it = map.iterator();
    var n: usize = 0;
    while (it.next()) |_| n += 1;
    try out.print("count={d} readback={d} get7={d} removed={d} mid={d} has4={d} has7={d} final={d} get4={d} iter={d}\n", .{
        @as(usize, 1000), readback, map.get(7).?, removed, @as(usize, 500),
        has4, has7, map.count(), map.get(4).?, n,
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "HashMap stress must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "count=1000 readback=1000 get7=7777 removed=500 mid=500 has4=0 has7=1 final=1000 get4=44 iter=1000\n"
    );
}

/// `std.StringHashMap(u32)` word-count via `getOrPut` (the in-place increment
/// primitive returning a pointer into the value storage), then a full iterate.
#[test]
fn run_std_string_hashmap_word_count_getorput() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer { const leaked = gpa.deinit(); if (leaked) @panic("leak"); }
    const alloc = gpa.allocator();
    var map = std.StringHashMap(u32).init(alloc);
    defer map.deinit();
    const words = [_][]const u8{ "apple", "banana", "apple", "fig", "banana", "apple" };
    var i: usize = 0;
    while (i < words.len) : (i += 1) {
        const r = try map.getOrPut(words[i]);
        if (r.found_existing) { r.value_ptr.* += 1; } else { r.value_ptr.* = 1; }
    }
    var total: u32 = 0;
    var it = map.iterator();
    while (it.next()) |e| total += e.value;
    try out.print("distinct={d} apple={d} banana={d} fig={d} missing={d} total={d}\n", .{
        map.count(), map.get("apple").?, map.get("banana").?, map.get("fig").?,
        map.get("grape") orelse 0, total,
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "string word-count must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "distinct=3 apple=3 banana=2 fig=1 missing=0 total=6\n"
    );
}

/// `std.sort.Sorter(T, Ctx)` sorts ascending AND descending in the same program
/// (distinct comptime contexts), with `isSorted` / `binarySearch` over the
/// result — the multi-context monomorphization that the per-instantiation member
/// resolution makes correct.
#[test]
fn run_std_sort_ascending_descending_and_search() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var xs = [_]i32{ 9, 3, 7, 1, 8, 2, 8, 0, 5, 3, 6, 4, 20, 11, 13, 19, 17, 2, 15, 0, 10, 12, 14, 18, 16, 1, 7, 6, 9, 5 };
    std.sort.Sorter(i32, std.sort.asc(i32)).sort(&xs);
    const asc_ok: u32 = if (std.sort.Sorter(i32, std.sort.asc(i32)).isSorted(&xs)) 1 else 0;
    const found: u32 = if (std.sort.Sorter(i32, std.sort.asc(i32)).binarySearch(&xs, 13) != null) 1 else 0;
    const absent: u32 = if (std.sort.Sorter(i32, std.sort.asc(i32)).binarySearch(&xs, 100) == null) 1 else 0;
    const asc_first = xs[0];
    const asc_last = xs[xs.len - 1];
    std.sort.Sorter(i32, std.sort.desc(i32)).sort(&xs);
    try out.print("asc_first={d} asc_last={d} sorted={d} found={d} absent={d} desc_first={d} desc_last={d}\n", .{
        asc_first, asc_last, asc_ok, found, absent, xs[0], xs[xs.len - 1],
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "sort must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "asc_first=0 asc_last=20 sorted=1 found=1 absent=1 desc_first=20 desc_last=0\n"
    );
}

/// `std.unicode`: count / decode / validate / encode over ASCII, 2/3/4-byte
/// sequences, and rejected invalid input.
#[test]
fn run_std_unicode_decode_validate_encode() {
    // The multi-byte inputs are spelled as explicit UTF-8 byte arrays (a Rust raw
    // byte-string literal cannot carry non-ASCII): "café" = 63 61 66 C3A9, the
    // euro sign U+20AC = E2 82 AC, the grinning-face emoji U+1F600 = F0 9F 98 80.
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const cafe = [_]u8{ 0x63, 0x61, 0x66, 0xC3, 0xA9 };
    const euro = [_]u8{ 0xE2, 0x82, 0xAC };
    const emoji = [_]u8{ 0xF0, 0x9F, 0x98, 0x80 };
    const cafe_n = std.unicode.utf8CountCodepoints(&cafe).?;
    const euro_n = std.unicode.utf8CountCodepoints(&euro).?;
    const emoji_n = std.unicode.utf8CountCodepoints(&emoji).?;
    const re = std.unicode.utf8DecodeAt(&emoji, 0);
    const bad = [_]u8{ 0x80, 0x41 };
    const trunc = [_]u8{ 0xE2, 0x82 };
    const bad_ok: u32 = if (std.unicode.utf8Validate(&bad)) 1 else 0;
    const trunc_ok: u32 = if (std.unicode.utf8Validate(&trunc)) 1 else 0;
    var eb: [4]u8 = undefined;
    const n = std.unicode.utf8Encode(0x20AC, &eb);
    try out.print("cafe={d} euro={d} emoji={d} emoji_cp={d} emoji_len={d} bad={d} trunc={d} enc_len={d} b0={d} b1={d} b2={d}\n", .{
        cafe_n, euro_n, emoji_n, re.cp, re.len, bad_ok, trunc_ok, n, eb[0], eb[1], eb[2],
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "unicode must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "cafe=4 euro=1 emoji=1 emoji_cp=128512 emoji_len=4 bad=0 trunc=0 enc_len=3 b0=226 b1=130 b2=172\n"
    );
}

/// std.unicode must REJECT ill-formed UTF-8 that the naive decoder used to
/// accept: overlong encodings (a codepoint encoded in more bytes than its
/// minimum form — a classic smuggling vector) and UTF-16 surrogates
/// (U+D800..U+DFFF, not valid scalar values). utf8Encode must also refuse to
/// emit a surrogate.
#[test]
fn run_std_unicode_rejects_overlong_and_surrogates() {
    // C0 80 = overlong U+0000; E0 80 AF = overlong '/'; F0 80 80 80 = overlong
    // U+0000 in 4 bytes; ED A0 80 = U+D800 surrogate; ED BF BF = U+DFFF surrogate.
    // C3 A9 = valid "é"; F0 9F 98 80 = valid emoji.
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const ol2 = [_]u8{ 0xC0, 0x80 };
    const ol3 = [_]u8{ 0xE0, 0x80, 0xAF };
    const ol4 = [_]u8{ 0xF0, 0x80, 0x80, 0x80 };
    const sur_lo = [_]u8{ 0xED, 0xA0, 0x80 };
    const sur_hi = [_]u8{ 0xED, 0xBF, 0xBF };
    const ok_e = [_]u8{ 0xC3, 0xA9 };
    const ok_em = [_]u8{ 0xF0, 0x9F, 0x98, 0x80 };
    var eb: [4]u8 = undefined;
    const enc_surrogate = std.unicode.utf8Encode(0xD800, &eb);
    const enc_ok = std.unicode.utf8Encode(0x20AC, &eb);
    try out.print("ol2={d} ol3={d} ol4={d} sl={d} sh={d} e={d} em={d} encsur={d} encok={d}\n", .{
        if (std.unicode.utf8Validate(&ol2)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&ol3)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&ol4)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&sur_lo)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&sur_hi)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&ok_e)) @as(u32, 1) else 0,
        if (std.unicode.utf8Validate(&ok_em)) @as(u32, 1) else 0,
        enc_surrogate, enc_ok,
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "unicode must exit 0; stderr: {stderr}");
    // All ill-formed inputs reject (0); valid ones accept (1); surrogate encode
    // returns 0 bytes; a valid encode returns 3.
    assert_eq!(
        stdout,
        "ol2=0 ol3=0 ol4=0 sl=0 sh=0 e=1 em=1 encsur=0 encok=3\n"
    );
}

/// `std.math` helpers and the fixed-width `std.Big` bignum (add + multiply +
/// to-decimal), checked against hand-computed values.
#[test]
fn run_std_math_and_bignum() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = std.Big.fromU64(4294967301);
    const b = std.Big.fromU64(4294967303);
    var buf1: [80]u8 = undefined;
    var buf2: [80]u8 = undefined;
    const sum = std.Big.add(a, b).toDecimal(&buf1);
    const mul = std.Big.mul(std.Big.fromU64(1000000), std.Big.fromU64(1000000)).toDecimal(&buf2);
    try out.print("abs={d} gcd={d} pow={d} lcm={d} clamp={d} min={d} max={d} bigsum={s} bigmul={s}\n", .{
        std.math.absI64(-7), std.math.gcd(48, 36), std.math.powU64(2, 10), std.math.lcm(4, 6),
        std.math.clamp(i64, 12, 0, 10), std.math.min(i64, 3, 9), std.math.max(i64, 3, 9), sum, mul,
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "math/bignum must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "abs=7 gcd=12 pow=1024 lcm=12 clamp=10 min=3 max=9 bigsum=8589934604 bigmul=1000000000000\n"
    );
}

/// New allocators: the `CountingAllocator` wrapper tallies alloc/free/bytes while
/// forwarding to its inner GPA (which then leak-checks clean), and the
/// `StackAllocator` (bump over a caller buffer) hands out and writes through
/// disjoint windows.
#[test]
fn run_std_counting_and_stack_allocators() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var gpa = std.heap.GeneralPurposeAllocator.init(sys);
    defer { const leaked = gpa.deinit(); if (leaked) @panic("leak"); }
    var c = std.heap.CountingAllocator.init(gpa.allocator());
    const a = try c.alloc(u8, 100);
    const b = try c.alloc(u8, 16);
    c.free(a);
    c.free(b);
    var sbuf: [128]u8 = undefined;
    var sa = std.heap.StackAllocator.init(&sbuf);
    const salloc = sa.allocator();
    const x = try salloc.alloc(u8, 32);
    const y = try salloc.alloc(u8, 16);
    x[0] = 7;
    y[0] = 9;
    try out.print("n_alloc={d} n_free={d} bytes={d} x.len={d} y.len={d} x0={d} y0={d}\n", .{
        c.n_alloc, c.n_free, c.bytes, x.len, y.len, x[0], y[0],
    });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "allocators must exit 0; stderr: {stderr}");
    assert_eq!(
        stdout,
        "n_alloc=2 n_free=2 bytes=116 x.len=32 y.len=16 x0=7 y0=9\n"
    );
}

/// A `[]const u8` slice built at run time (here a `std.Big.toDecimal` digit run
/// over a stack buffer) renders correctly under the `{s}` verb — the heap-backed
/// byte-slice the format engine now materializes to its bytes.
#[test]
fn run_runtime_byte_slice_renders_as_string() {
    let src = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var buf: [16]u8 = undefined;
    buf[0] = 'h';
    buf[1] = 'i';
    const s: []const u8 = buf[0..2];
    try out.print("s={s} len={d}\n", .{ s, s.len });
}
"#;
    let (code, stdout, stderr) = run_with_code(&["run", "-"], src);
    assert_eq!(code, 0, "byte-slice render must exit 0; stderr: {stderr}");
    assert_eq!(stdout, "s=hi len=2\n");
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

// =========================================================================
//  `k2c lsp` — the language server over real stdio
// =========================================================================

/// Frames a JSON-RPC body as a `Content-Length` message.
fn lsp_frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

#[test]
fn lsp_initialize_smoke_returns_capabilities() {
    // The minimal acceptance: an initialize request over stdio returns a result
    // advertising capabilities. Closing stdin ends the server cleanly.
    let mut session = String::new();
    session.push_str(&lsp_frame(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    ));
    let (_ok, stdout, _stderr) = run_with_stdin(&["lsp"], session.as_bytes());
    assert!(
        stdout.contains("capabilities"),
        "initialize response advertises capabilities: {stdout}"
    );
    assert!(
        stdout.contains("hoverProvider"),
        "initialize advertises hoverProvider: {stdout}"
    );
}

#[test]
fn lsp_scripted_session_over_stdio() {
    // A full scripted session through the *real* `k2c lsp` binary: initialize,
    // didOpen a .k2 doc, hover, definition, completion, formatting, shutdown.
    let uri = "file:///session.k2";
    // The doc text, JSON-escaped for embedding in a request body.
    let doc = "const x: i32 = 1;\\npub fn main() void {\\n    const y = x;\\n}\\n";

    let mut s = String::new();
    s.push_str(&lsp_frame(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    ));
    s.push_str(&lsp_frame(
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    ));
    s.push_str(&lsp_frame(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"k2","version":1,"text":"{doc}"}}}}}}"#
    )));
    // hover/definition on the `x` use at line 2, char 14.
    s.push_str(&lsp_frame(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":2,"character":14}}}}}}"#
    )));
    s.push_str(&lsp_frame(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"textDocument/definition","params":{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":2,"character":14}}}}}}"#
    )));
    s.push_str(&lsp_frame(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"textDocument/completion","params":{{"textDocument":{{"uri":"{uri}"}},"position":{{"line":2,"character":14}}}}}}"#
    )));
    s.push_str(&lsp_frame(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"textDocument/formatting","params":{{"textDocument":{{"uri":"{uri}"}},"options":{{}}}}}}"#
    )));
    s.push_str(&lsp_frame(
        r#"{"jsonrpc":"2.0","id":6,"method":"shutdown"}"#,
    ));
    s.push_str(&lsp_frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let (ok, stdout, _stderr) = run_with_stdin(&["lsp"], s.as_bytes());
    assert!(ok, "clean exit after shutdown/exit");

    // publishDiagnostics arrived (the clean doc → empty list).
    assert!(
        stdout.contains("textDocument/publishDiagnostics"),
        "diagnostics published: {stdout}"
    );
    // hover returned a type containing i32.
    assert!(stdout.contains("i32"), "hover/type present: {stdout}");
    // completion returned candidates (the `main` item is visible).
    assert!(
        stdout.contains("\"main\""),
        "completion candidates: {stdout}"
    );
    // formatting returned the canonical newText.
    assert!(
        stdout.contains("newText"),
        "formatting edit present: {stdout}"
    );
}

// =========================================================================
//  v0.17 — driver-level acceptance: run-native (every mode) == run (VM)
// =========================================================================

/// Runs `k2c` with `args` and returns `(exit_code, raw_stdout, raw_stderr)`. Passes
/// a real file path (no stdin), so the child's exit code and byte-exact streams are
/// captured for the differential comparison.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn run_native_cli(args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let out = k2c().args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

/// The v0.22 std containers/algorithms whose monomorphized helper bodies carry an
/// unresolved (`deferred`) generic element type — `std.HashMap` (parallel slices)
/// and `std.sort.Sorter` (its private `quick`/`insertionRange` helpers index a
/// `[]T` with `T` still generic) — are CLEANLY REFUSED by the native backend
/// (with the "run it on the VM" note) rather than miscompiled to wrong results.
/// The VM is the semantic reference for these; the `run_std_*` tests above prove
/// they compute correctly there.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn run_native_refuses_std_hashmap_and_sort_cleanly() {
    let hashmap = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const alloc = sys.heap;
    var map = std.IntHashMap(u32, u64).init(alloc);
    defer map.deinit();
    try map.put(1, 100);
    _ = map.get(1);
}
"#;
    let (code, _out, err) = run_with_code(&["run-native", "-"], hashmap);
    assert_ne!(code, 0, "native HashMap must be refused, not run");
    assert!(
        err.contains("run it on the VM") || err.contains("native backend"),
        "native HashMap refusal must name the VM fallback, got: {err}"
    );

    let sort = br#"
const std = @import("std");
pub fn main(sys: *System) !void {
    var xs = [_]i32{ 3, 1, 2 };
    std.sort.Sorter(i32, std.sort.asc(i32)).sort(&xs);
    _ = xs;
}
"#;
    let (code, _out, err) = run_with_code(&["run-native", "-"], sort);
    assert_ne!(code, 0, "native std sort must be refused, not miscompiled");
    assert!(
        err.contains("run it on the VM") || err.contains("native backend"),
        "native sort refusal must name the VM fallback, got: {err}"
    );
}

/// **HARD ACCEPTANCE**: `k2c run-native` in `--debug`, `--release-safe`, and
/// `--release-fast` produces stdout + exit byte-identical to `k2c run` (the VM) in
/// the same mode, and identical to native `--debug`, for hello/errors/allocators.
/// This is the literal milestone criterion, asserted by running real binaries.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn run_native_matches_vm_in_every_mode() {
    for example in ["hello", "errors", "allocators"] {
        let path = examples_dir().join(format!("{example}.k2"));
        let path = path.to_str().unwrap();

        // The VM reference (Debug); the program is deterministic, so the VM output
        // is the same across modes for these non-trapping examples.
        let (vm_code, vm_out, _vm_err) = run_native_cli(&["run", path]);
        let (nd_code, nd_out, _nd_err) = run_native_cli(&["run-native", "--debug", path]);

        for mode in ["--debug", "--release-safe", "--release-fast"] {
            let (vc, vout, _ve) = run_native_cli(&["run", mode, path]);
            let (nc, nout, _ne) = run_native_cli(&["run-native", mode, path]);

            // native(mode) == VM(mode).
            assert_eq!(
                nout, vout,
                "[{example} {mode}] native stdout must equal VM stdout"
            );
            assert_eq!(nc, vc, "[{example} {mode}] native exit must equal VM exit");

            // native(mode) == native(Debug) == VM(Debug): these examples do not
            // trap, so optimization is fully behavior-preserving across modes.
            assert_eq!(
                nout, nd_out,
                "[{example} {mode}] native release stdout must equal native Debug"
            );
            assert_eq!(nout, vm_out, "[{example} {mode}] native must equal the VM");
            let _ = (nd_code, vm_code);
        }
    }
}

/// The `bench` subcommand prints the native-vs-VM speedup line. A light smoke test
/// that the harness runs end-to-end and reports a speedup (the non-flaky numeric
/// floor lives in the codegen test `native_is_much_faster_than_vm`).
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn bench_native_reports_speedup() {
    let out = k2c().args(["bench", "--native"]).output().unwrap();
    assert!(out.status.success(), "bench --native should succeed");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("speedup") && stdout.contains("faster than the VM"),
        "bench --native must report the native-vs-VM speedup, got:\n{stdout}"
    );
}

/// `build-native --target=aarch64-linux` cross-compiles a hello-class program to
/// a valid EM_AARCH64 static ELF. This validates the cross-compilation wiring
/// end-to-end via the driver and (when the host has `readelf`/`file`) confirms the
/// emitted file is what the standard tools recognize as an ARM aarch64 executable.
///
/// HONEST NOTE: the aarch64 binary is NEVER executed here — there is no
/// `qemu-aarch64` and no aarch64 hardware. The test validates the ELF
/// *structurally* (header bytes + `readelf`/`file`), exactly as the milestone's
/// verification constraint requires.
#[test]
fn build_native_aarch64_cross_compiles_valid_elf() {
    let src = examples_dir().join("hello.k2");
    let out_path = std::env::temp_dir().join(format!("k2c_hello_aarch64_{}", std::process::id()));
    let out = k2c()
        .args(["build-native", "--target=aarch64-linux"])
        .arg(&src)
        .arg("-o")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build-native --target=aarch64-linux should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&out_path).expect("aarch64 ELF written");
    // Structural header check (host-independent): EM_AARCH64 (183), ET_EXEC, entry.
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2, "ET_EXEC");
    assert_eq!(
        u16::from_le_bytes([bytes[18], bytes[19]]),
        183,
        "e_machine == EM_AARCH64"
    );

    // External tool validation, only where binutils/file are present.
    if let Ok(re) = Command::new("readelf").arg("-h").arg(&out_path).output() {
        if re.status.success() {
            let s = String::from_utf8_lossy(&re.stdout);
            assert!(
                s.contains("AArch64"),
                "readelf -h should report Machine: AArch64, got:\n{s}"
            );
        }
    }
    if let Ok(f) = Command::new("file").arg(&out_path).output() {
        if f.status.success() {
            let s = String::from_utf8_lossy(&f.stdout);
            assert!(
                s.contains("aarch64") || s.contains("ARM"),
                "file should report an ARM aarch64 executable, got:\n{s}"
            );
        }
    }
    let _ = std::fs::remove_file(&out_path);
}

/// `build-native --target=x86_64-linux` (the explicit default) produces the SAME
/// bytes as the implicit default, proving the default-target wiring is a no-op.
#[test]
fn build_native_explicit_x86_target_matches_default() {
    let src = examples_dir().join("hello.k2");
    let dir = std::env::temp_dir();
    let a = dir.join(format!("k2c_def_{}", std::process::id()));
    let b = dir.join(format!("k2c_x86_{}", std::process::id()));
    let r1 = k2c()
        .args(["build-native"])
        .arg(&src)
        .arg("-o")
        .arg(&a)
        .output()
        .unwrap();
    let r2 = k2c()
        .args(["build-native", "--target=x86_64-linux"])
        .arg(&src)
        .arg("-o")
        .arg(&b)
        .output()
        .unwrap();
    assert!(r1.status.success() && r2.status.success());
    let ba = std::fs::read(&a).unwrap();
    let bb = std::fs::read(&b).unwrap();
    assert_eq!(
        ba, bb,
        "explicit x86_64-linux must equal the default output"
    );
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// `run-native --target=aarch64-linux` refuses to execute a foreign-ISA binary on
/// this host with an actionable message (it cannot be run; no emulator).
#[test]
fn run_native_aarch64_refuses_on_x86_host() {
    let src = examples_dir().join("hello.k2");
    let out = k2c()
        .args(["run-native", "--target=aarch64-linux"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "run-native of a foreign target must fail"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cannot execute") && err.contains("aarch64"),
        "refusal message should explain the host mismatch, got:\n{err}"
    );
}

/// An unknown `--target` triple is rejected with a message listing the supported
/// triples.
#[test]
fn build_native_unknown_target_errors() {
    let src = examples_dir().join("hello.k2");
    let out = k2c()
        .args(["build-native", "--target=sparc-solaris"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown target must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("x86_64-linux") && err.contains("aarch64-linux"),
        "error should list supported triples, got:\n{err}"
    );
}

// =========================================================================
//  v0.19 C-interop / FFI driver tests (gated on a system `cc`)
// =========================================================================

/// Probes for a usable C compiler (`$CC`, then `cc`, then `gcc`); returns the
/// first whose `--version` runs, so an FFI test can skip cleanly without one.
fn find_cc() -> Option<String> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(cc) = std::env::var("CC") {
        if !cc.is_empty() {
            cands.push(cc);
        }
    }
    cands.push("cc".to_string());
    cands.push("gcc".to_string());
    for c in cands {
        let ok = Command::new(&c)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(c);
        }
    }
    None
}

/// Writes `src` into a fresh temp file under `dir` and returns its path.
fn write_k2(dir: &std::path::Path, name: &str, src: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, src).unwrap();
    p
}

/// `run-native --link-libc` on a puts-calling program prints the line and exits 0.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn run_native_link_libc_puts() {
    if find_cc().is_none() {
        eprintln!("skipping run_native_link_libc_puts: no C compiler");
        return;
    }
    let dir = std::env::temp_dir().join(format!("k2_ffi_run_{}", std::process::id()));
    let path = write_k2(
        &dir,
        "puts.k2",
        "extern fn puts(s: [*:0]const u8) c_int; \
         pub fn main() c_int { _ = puts(\"hi from libc\"); return 0; }",
    );
    let out = k2c()
        .args(["run-native", "--link-libc"])
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "run-native --link-libc must exit 0");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi from libc\n",
        "puts output on stdout"
    );
}

/// `build-native --link-libc` writes a runnable executable linked against libc.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn build_native_link_libc_writes_runnable() {
    if find_cc().is_none() {
        eprintln!("skipping build_native_link_libc_writes_runnable: no C compiler");
        return;
    }
    let dir = std::env::temp_dir().join(format!("k2_ffi_build_{}", std::process::id()));
    let src = write_k2(
        &dir,
        "puts.k2",
        "extern fn puts(s: [*:0]const u8) c_int; \
         pub fn main() c_int { _ = puts(\"built\"); return 0; }",
    );
    let exe = dir.join("puts");
    let out = k2c()
        .args(["build-native", "--link-libc", "-o"])
        .arg(&exe)
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build-native --link-libc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(run.status.success(), "linked binary must run + exit 0");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "built\n");
}

/// A `--link-libc` request with `$CC` pointing at a missing binary produces an
/// actionable error (and a nonzero exit), not a crash/panic.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn link_libc_missing_cc_is_actionable_error() {
    let dir = std::env::temp_dir().join(format!("k2_ffi_nocc_{}", std::process::id()));
    let src = write_k2(
        &dir,
        "puts.k2",
        "extern fn puts(s: [*:0]const u8) c_int; \
         pub fn main() c_int { _ = puts(\"x\"); return 0; }",
    );
    let out = k2c()
        .args(["build-native", "--link-libc"])
        .arg(&src)
        .env("CC", "/nonexistent/definitely-not-a-compiler-xyz")
        .env("PATH", "/nonexistent-empty-path")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.status.success(),
        "a missing C toolchain must fail, not succeed"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("C compiler") || err.contains("link"),
        "error should mention the missing C compiler, got:\n{err}"
    );
}

/// The freestanding (no `--link-libc`) native path still works for a non-FFI
/// program: it builds + runs without a C toolchain.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn freestanding_native_still_works_without_libc() {
    let path = examples_dir().join("hello.k2");
    let out = k2c().args(["run-native"]).arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "freestanding run-native must still work, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_eof_unterminated_const_no_std_leak() {
    // A truncated `const` initializer makes the parser run off the end of the
    // user source into the appended std prelude. The diagnostic must point at the
    // user's real last line (line 2), show a clean `found end of input`, and never
    // leak the internal `__k2_std_root` name or a phantom std line number.
    let (ok, _out, err) =
        run_with_stdin(&["check", "-"], b"pub fn main() void {\n    const x: i32 =");
    assert!(!ok, "an unterminated const must fail to check");
    assert!(
        !err.contains("__k2_std_root"),
        "std-root name leaked into a user diagnostic:\n{err}"
    );
    // No phantom std line numbers (std is ~590 lines; phantom lines were ~595).
    assert!(
        !err.contains("<stdin>:595:") && !err.contains("<stdin>:597:"),
        "phantom std line number leaked:\n{err}"
    );
    assert!(
        err.contains("found end of input"),
        "expected a clean end-of-input message:\n{err}"
    );
    // The locator points at the user's real last line (line 2) and shows it.
    assert!(
        err.contains("<stdin>:2:"),
        "locator not on user line 2:\n{err}"
    );
    assert!(
        err.contains("const x: i32 ="),
        "snippet must show the user's last line:\n{err}"
    );
}

#[test]
fn check_eof_unclosed_brace_no_std_leak() {
    // An unclosed function body brace runs the parser into std at EOF. Same
    // contract: real user line (line 1), clean message, no std leak.
    let (ok, _out, err) = run_with_stdin(&["check", "-"], b"pub fn f() void {");
    assert!(!ok, "an unclosed brace must fail to check");
    assert!(
        !err.contains("__k2_std_root"),
        "std-root name leaked:\n{err}"
    );
    assert!(
        !err.contains("<stdin>:595:"),
        "phantom std line leaked:\n{err}"
    );
    // The caret/locator points at the user's only line (line 1).
    assert!(
        err.contains("<stdin>:1:"),
        "locator not on user line 1:\n{err}"
    );
    assert!(
        err.contains("pub fn f() void {"),
        "snippet must show the user's line:\n{err}"
    );
}

#[test]
fn run_prints_error_return_trace_through_try_sites() {
    // A program propagating an error through `try a()` (in main) and `try b()`
    // (in a) out of main prints an error-return trace listing those sites,
    // newest-first, via `k2c run`.
    let dir = std::env::temp_dir().join(format!("k2_trace_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = r#"const std = @import("std");
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
    let path = dir.join("trace.k2");
    std::fs::write(&path, src).unwrap();

    let out = k2c().arg("run").arg(&path).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the error must escape nonzero");
    assert!(stderr.contains("error: Boom"), "stderr: {stderr}");
    assert!(
        stderr.contains("error return trace:"),
        "expected a trace block, got: {stderr}"
    );
    assert!(
        stderr.contains("at b ("),
        "trace must list the origin `b` (return Boom.Boom), got: {stderr}"
    );
    assert!(
        stderr.contains("at a ("),
        "trace must list `a`, got: {stderr}"
    );
    assert!(
        stderr.contains("at main ("),
        "trace must list `main`, got: {stderr}"
    );
    // Newest-first: the origin `b` frame is deepest (first), then `a`, then
    // `main`.
    let bi = stderr.find("at b (").unwrap();
    let ai = stderr.find("at a (").unwrap();
    let mi = stderr.find("at main (").unwrap();
    assert!(bi < ai && ai < mi, "trace not newest-first:\n{stderr}");
    // The trace locations reference the real source file.
    assert!(
        stderr.contains("trace.k2:"),
        "trace must point at the source file, got: {stderr}"
    );

    // In ReleaseFast the trace block is stripped; only the header prints.
    let out_rf = k2c()
        .arg("run")
        .arg("--release-fast")
        .arg(&path)
        .output()
        .unwrap();
    let stderr_rf = String::from_utf8_lossy(&out_rf.stderr);
    assert!(stderr_rf.contains("error: Boom"), "stderr: {stderr_rf}");
    assert!(
        !stderr_rf.contains("error return trace:"),
        "ReleaseFast must strip the trace, got: {stderr_rf}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_rich_caret_aligns_under_multibyte_token() {
    // A multi-byte string literal (`"café 漢字"`) precedes the offending value on
    // the same line, so a byte-based caret would be misplaced. The rich renderer
    // must align the `^` under the *display* column of `true`, counting `é` as
    // one cell and each CJK glyph as two.
    let src = "fn f() void { const s = \"café 漢字\"; _ = s; const x: u8 = true; _ = x; }\n";
    let (ok, _stdout, stderr) = run_with_stdin(&["check", "-"], src.as_bytes());
    assert!(!ok);
    assert!(stderr.contains("expected `u8`, found `bool`"), "{stderr}");
    assert!(stderr.contains("-->"), "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let si = lines
        .iter()
        .position(|l| l.contains("café"))
        .expect("source line");
    let src_line = lines[si];
    let underline = lines[si + 1];
    let caret = underline.find('^').expect("caret");
    let ubar = underline.find('|').unwrap();
    let sbar = src_line.find('|').unwrap();
    // The expected caret column is the display width of the text before `true`:
    // each `é` is one cell, each CJK glyph is two.
    let text_after_bar = &src_line[sbar + 2..];
    let true_byte = text_after_bar.find("true").unwrap();
    let prefix = &text_after_bar[..true_byte];
    let true_disp: usize = prefix
        .chars()
        .map(|c| {
            let cp = c as u32;
            if (0x4E00..=0x9FFF).contains(&cp) {
                2
            } else {
                1
            }
        })
        .sum();
    assert_eq!(
        caret - (ubar + 2),
        true_disp,
        "caret must align under `true` past the multi-byte text\n{stderr}"
    );
    // Sanity: the byte offset of `true` differs from its display column.
    assert_ne!(
        true_disp, true_byte,
        "fixture must exercise multi-byte width"
    );
}

#[test]
fn check_rich_caret_copies_leading_tab() {
    // A real tab indents the offending line; the rich renderer copies the tab
    // verbatim into the underline row so the caret aligns at any tab width.
    let src = "fn f() void {\n\tconst x: u8 = true;\n}\n";
    let (ok, _stdout, stderr) = run_with_stdin(&["check", "-"], src.as_bytes());
    assert!(!ok);
    assert!(stderr.contains("expected `u8`, found `bool`"), "{stderr}");
    let underline = stderr
        .lines()
        .find(|l| l.contains('^'))
        .expect("underline row");
    let after_bar = &underline[underline.find('|').unwrap() + 2..];
    assert!(
        after_bar.starts_with('\t'),
        "underline must copy the leading tab verbatim, got: {after_bar:?}"
    );
}
