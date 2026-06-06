//! v0.30 (1.0 readiness) — the spec-conformance suite.
//!
//! The committed corpus under `conformance/<chapter>/` is a set of small,
//! deterministic k2 programs spanning the language spec (§01 lexical structure →
//! §10 standard library), each paired with its captured stdout (`<case>.expected`).
//! This harness runs every case and asserts the toolchain reproduces that output
//! exactly — on the bytecode **VM** for all cases, and additionally on the
//! **native** x86-64 backend for any case carrying a `<case>.native` marker
//! (i.e. one whose native output is byte-identical to the VM's).
//!
//! The corpus is authored to be reproducible: no clock, randomness, addresses,
//! or unordered iteration in the asserted output. A failure here means either a
//! real toolchain regression or a non-deterministic case that slipped in.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("conformance")
        .canonicalize()
        .expect("canonicalize conformance dir")
}

/// Every `*.k2` case under `conformance/`, sorted for stable iteration.
fn collect_cases(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_cases(&p, out);
        } else if p.extension().is_some_and(|e| e == "k2") {
            out.push(p);
        }
    }
}

/// Runs `k2c <subcmd> <file>`, returning (exit_code, stdout). Retries only on a
/// transient native-exec ETXTBSY.
fn run(subcmd: &str, file: &Path) -> (i32, String) {
    let k2c = env!("CARGO_BIN_EXE_k2c");
    let mut attempt = 0;
    loop {
        let out = Command::new(k2c)
            .arg(subcmd)
            .arg(file)
            .output()
            .expect("spawn k2c");
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if code != 0
            && (stderr.contains("Text file busy") || stderr.contains("ETXTBSY"))
            && attempt < 50
        {
            attempt += 1;
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        return (code, String::from_utf8_lossy(&out.stdout).into_owned());
    }
}

#[test]
fn conformance_corpus_reproduces_on_the_vm() {
    let root = conformance_dir();
    let mut cases = Vec::new();
    collect_cases(&root, &mut cases);
    assert!(
        cases.len() >= 50,
        "conformance corpus shrank unexpectedly ({} cases) — did the corpus get truncated?",
        cases.len()
    );

    for case in &cases {
        let expected = std::fs::read_to_string(case.with_extension("expected"))
            .unwrap_or_else(|e| panic!("read .expected for {}: {e}", case.display()));
        let (code, stdout) = run("run", case);
        assert_eq!(code, 0, "VM `{}` must exit 0", case.display());
        assert_eq!(
            stdout,
            expected,
            "VM output for `{}` diverged from its captured .expected",
            case.display()
        );
    }
}

#[test]
fn conformance_native_marked_cases_match_the_vm() {
    let root = conformance_dir();
    let mut cases = Vec::new();
    collect_cases(&root, &mut cases);

    let mut native_checked = 0;
    for case in &cases {
        // Only cases explicitly marked native-capable (byte-identical to the VM).
        if !case.with_extension("native").exists() {
            continue;
        }
        let expected = std::fs::read_to_string(case.with_extension("expected"))
            .unwrap_or_else(|e| panic!("read .expected for {}: {e}", case.display()));
        let (code, stdout) = run("run-native", case);
        assert_eq!(code, 0, "native `{}` must exit 0", case.display());
        assert_eq!(
            stdout,
            expected,
            "native output for `{}` diverged from the VM/.expected",
            case.display()
        );
        native_checked += 1;
    }
    assert!(
        native_checked >= 30,
        "expected a substantial native-verified subset, only checked {native_checked}"
    );
}
