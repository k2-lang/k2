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
