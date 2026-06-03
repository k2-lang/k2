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
