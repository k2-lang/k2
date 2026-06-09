//! v0.30 (1.0 readiness) — the full example integration sweep.
//!
//! This pins the authoritative "what runs where" matrix for the shipped
//! examples, so a regression in either backend is caught:
//!
//!  * Every runnable example runs on the **VM** (`k2c run`): exits 0, prints
//!    non-empty output, and is **deterministic** (two runs are byte-identical).
//!  * The native-capable examples run on the **native x86-64 backend**
//!    (`k2c run-native`) with output **byte-identical to the VM** — the v0.16/
//!    v0.17 differential contract.
//!  * The examples that use capabilities outside the native subset (generic
//!    containers with aggregate elements, fibers, fs/net/time syscalls) are
//!    **cleanly refused** by the native backend — a nonzero exit with a
//!    "run it on the VM" note, never a miscompile.
//!  * `comptime_reflection.k2` is a *comptime-reflection demo*: the front-end
//!    accepts it (`k2c check` succeeds), but executing it reaches a documented
//!    limitation — runtime `inline for` over reflected fields (post-1.0) — which
//!    the VM reports as a **clean, controlled panic-trap** (exit 134 + a
//!    `panic:` line), never a Rust abort. This test pins exactly that contract.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .canonicalize()
        .expect("canonicalize examples dir")
}

/// Runs `k2c <subcmd> examples/<name>.k2`, returning (exit_code, stdout, stderr).
/// Retries only on a transient native-exec ETXTBSY ("Text file busy").
fn run_example(subcmd: &str, name: &str) -> (i32, String, String) {
    let path = examples_dir().join(format!("{name}.k2"));
    let k2c = env!("CARGO_BIN_EXE_k2c");
    let mut attempt = 0;
    loop {
        let out = Command::new(k2c)
            .arg(subcmd)
            .arg(&path)
            .output()
            .expect("spawn k2c");
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if code != 0
            && (stderr.contains("Text file busy") || stderr.contains("ETXTBSY"))
            && attempt < 50
        {
            attempt += 1;
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        return (code, stdout, stderr);
    }
}

/// Examples that run to completion on the VM (the semantic reference).
const VM_RUNNABLE: &[&str] = &[
    "hello",
    "errors",
    "allocators",
    "unions",
    "generic_list",
    "data_structures",
    "concurrency",
    "os_capabilities",
];

/// Examples whose native binary matches the VM byte-for-byte (the differential
/// contract). The rest are cleanly refused by the native subset. `unions` uses
/// only scalar union payloads, which the native backend fully supports.
const NATIVE_PARITY: &[&str] = &["hello", "errors", "allocators", "unions"];

/// Examples the native backend must cleanly REFUSE (outside the native subset),
/// never miscompile. (They all run on the VM.)
const NATIVE_REFUSED: &[&str] = &[
    "generic_list",    // generic-helper body indexing a still-generic slice
    "data_structures", // HashMap with aggregate (`[]const u8`) keys
    "concurrency",     // cooperative fibers (VM-only scheduler)
    "os_capabilities", // fs/net/time syscalls outside the native subset
];

#[test]
fn every_example_runs_deterministically_on_the_vm() {
    for &name in VM_RUNNABLE {
        let (code, out1, err) = run_example("run", name);
        assert_eq!(code, 0, "`k2c run {name}` must exit 0; stderr:\n{err}");
        assert!(!out1.is_empty(), "`{name}` must print something on the VM");
        let (_c2, out2, _e2) = run_example("run", name);
        assert_eq!(
            out1, out2,
            "`{name}` output must be deterministic across runs"
        );
    }
}

#[test]
fn native_capable_examples_match_the_vm() {
    for &name in NATIVE_PARITY {
        let (vm_code, vm_out, vm_err) = run_example("run", name);
        assert_eq!(vm_code, 0, "VM `{name}` must exit 0; stderr:\n{vm_err}");
        let (nat_code, nat_out, nat_err) = run_example("run-native", name);
        assert_eq!(
            nat_code, 0,
            "native `{name}` must exit 0; stderr:\n{nat_err}"
        );
        assert_eq!(
            nat_out, vm_out,
            "native `{name}` output must be byte-identical to the VM"
        );
    }
}

#[test]
fn native_subset_cleanly_refuses_the_rest() {
    for &name in NATIVE_REFUSED {
        // It runs on the VM …
        let (vm_code, _vm_out, vm_err) = run_example("run", name);
        assert_eq!(vm_code, 0, "VM `{name}` must exit 0; stderr:\n{vm_err}");
        // … but native refuses it cleanly (nonzero + a VM-fallback note), never
        // emits a wrong binary.
        let (nat_code, _nat_out, nat_err) = run_example("run-native", name);
        assert_ne!(nat_code, 0, "native `{name}` must be refused, not run");
        let refused = nat_err.contains("run it on the VM")
            || nat_err.contains("native backend")
            || nat_err.contains("unsupported");
        assert!(
            refused,
            "native refusal of `{name}` must name the VM fallback / unsupported feature; got:\n{nat_err}"
        );
    }
}

/// `comptime_reflection.k2` type-checks cleanly (the reflection front-end is
/// sound), and running it reaches a documented runtime limitation as a clean,
/// controlled panic-trap (exit 134 + a `panic:` line) — never a Rust abort.
#[test]
fn comptime_reflection_typechecks_and_traps_cleanly() {
    let (check_code, _o, check_err) = run_example("check", "comptime_reflection");
    assert_eq!(
        check_code, 0,
        "the reflection demo must type-check; stderr:\n{check_err}"
    );

    let (run_code, _ro, run_err) = run_example("run", "comptime_reflection");
    assert_eq!(
        run_code, 134,
        "the reflection demo must reach its documented runtime limit as a clean panic-trap (exit 134); stderr:\n{run_err}"
    );
    assert!(
        run_err.contains("panic:"),
        "the trap must be the controlled VM panic-trap (a `panic:` line), not a silent crash; got:\n{run_err}"
    );
}
