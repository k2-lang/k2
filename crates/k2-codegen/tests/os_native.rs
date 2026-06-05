//! v0.23 native OS-effect tests: the feasible `sys.os` subset (`getpid`/`exit`)
//! that lowers to raw Linux syscalls, plus the CLEAN-REFUSAL boundary for the
//! capabilities the native subset does not yet implement (`sys.fs`/`sys.net`/
//! `sys.time` and `os.args`/`env.get`).
//!
//! The deliverable's contract for native is: implement what is feasible with raw
//! syscalls, and CLEANLY REFUSE the rest (`CodegenError::Unsupported`) so the
//! program runs on the VM — NEVER a miscompile. These tests pin both halves: the
//! `getpid`/`exit` program both compiles AND runs (exit code asserted), and an
//! `fs`/`net`/`time` program is rejected at compile time with the documented
//! "intrinsic ... unsupported" message (never silently producing a wrong binary).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use k2_codegen::compile_program_to_elf;
use k2_mir::{lower_program, BuildMode, MirProgram};

/// Lowers a self-contained k2 source string (with the std prelude injected) to a
/// verified `MirProgram`, mirroring the `k2c` driver.
fn lower(source: &str) -> MirProgram {
    let mut combined = String::from(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());
    let mut pres = k2_parse::parse(&combined);
    rewrite_std_imports(&mut pres.file);
    assert!(pres.is_ok(), "parse errors in test program");
    let resolved = k2_resolve::resolve_file(&pres.file);
    assert!(resolved.is_ok(), "resolution errors in test program");
    let typed = k2_types::check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors in test program");
    let prog =
        lower_program(&pres.file, &resolved, typed, BuildMode::Debug).expect("lowering failed");
    let problems = prog.verify();
    assert!(problems.is_empty(), "malformed MIR: {problems:?}");
    prog
}

/// Re-points `const X = @import("std")` to the synthetic std root.
fn rewrite_std_imports(file: &mut k2_syntax::SourceFile) {
    use k2_syntax::{Expr, Item};
    for item in &mut file.items {
        if let Item::Const { value, .. } = item {
            let is_std = matches!(
                value,
                Expr::Builtin { name, args, .. }
                    if name == "@import"
                        && matches!(args.as_slice(), [Expr::Str { text, .. }] if text.trim_matches('"') == "std")
            );
            if is_std {
                let span = value.span();
                *value = Expr::Ident {
                    name: k2_std::STD_ROOT_NAME.to_string(),
                    span,
                };
            }
        }
    }
}

/// `true` on an x86-64 Linux host, where the emitted ELF can actually execute.
fn can_run_native() -> bool {
    cfg!(target_arch = "x86_64") && cfg!(target_os = "linux")
}

/// Writes the ELF image to a unique temp path, `chmod +x`-es it, runs it, and
/// returns the process exit code. Self-cleaning (the temp binary is removed).
fn build_and_run(prog: &MirProgram) -> i32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let img = match compile_program_to_elf(prog) {
        Ok(img) => img,
        Err(e) => panic!("native compile must succeed for this program: {e}"),
    };
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("k2_v23_native_{}_{}", std::process::id(), n));
    std::fs::write(&path, &img.bytes).expect("write temp binary");
    // chmod 0o755.
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    // Executing a freshly-written file can transiently fail with ETXTBSY
    // ("Text file busy", errno 26) when another test thread's fork+exec still
    // holds a writable fd to this just-written binary; retry with a short
    // backoff (matching the run_native helper in src/tests.rs).
    let mut attempt = 0;
    let status = loop {
        match std::process::Command::new(&path).status() {
            Ok(s) => break s,
            Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("run binary: {e:?}"),
        }
    };
    let _ = std::fs::remove_file(&path);
    status.code().unwrap_or(-1)
}

// =========================================================================
//  The feasible native subset: getpid + exit (raw syscalls)
// =========================================================================

#[test]
fn native_os_getpid_and_exit() {
    // `getpid()` returns the real (positive) pid via syscall 39; `exit(code)` ends
    // the process via syscall 60. The program exits 7 iff `getpid() > 0`, so the
    // observed exit code pins both syscalls. The VM agrees (it exits 7 too), but it
    // returns a deterministic pid `1` — both satisfy the `> 0` predicate, which is
    // the only thing a portable test ever asserts about a pid.
    let prog = lower(
        r#"
pub fn main(sys: *System) i32 {
    if (sys.os.getpid() > 0) {
        sys.os.exit(7);
    }
    return 0;
}
"#,
    );
    if !can_run_native() {
        // Off-host: still assert the program is IN the native subset (it compiles).
        assert!(compile_program_to_elf(&prog).is_ok());
        return;
    }
    assert_eq!(build_and_run(&prog), 7, "getpid>0 then exit(7)");
}

#[test]
fn native_os_exit_propagates_code() {
    let prog = lower(
        r#"
pub fn main(sys: *System) i32 {
    sys.os.exit(42);
    return 0;
}
"#,
    );
    if !can_run_native() {
        assert!(compile_program_to_elf(&prog).is_ok());
        return;
    }
    assert_eq!(build_and_run(&prog), 42);
}

// =========================================================================
//  The clean-refusal boundary: fs / net / time are VM-only (refused, not miscompiled)
// =========================================================================

/// Every program in this list uses a v0.23 capability the native subset does NOT
/// implement; the native backend must refuse it cleanly (`Err`), never emit a wrong
/// binary. Each refusal is surfaced by the driver with the "run it on the VM" note.
#[test]
fn native_refuses_fs_net_time_cleanly() {
    let cases = [
        // sys.fs: needs a path/buffer scratch the native subset does not carry.
        r#"pub fn main(sys: *System) !void {
            var f = try sys.fs.create("/tmp/k2_v23_never.tmp");
            _ = try f.write("x");
            f.close();
        }"#,
        // sys.net: needs a sockaddr scratch + socket syscalls.
        r#"pub fn main(sys: *System) !void {
            var l = try sys.net.listen(0);
            l.close();
        }"#,
        // sys.time real clocks: need a timespec scratch.
        r#"pub fn main(sys: *System) !void {
            const out = sys.io.stdout();
            try out.print("{d}\n", .{sys.time.monotonicReal()});
        }"#,
    ];
    for src in cases {
        let prog = lower(src);
        match compile_program_to_elf(&prog) {
            Ok(_) => panic!("native backend must REFUSE this v0.23 program (VM-only)"),
            Err(e) => {
                // The refusal names the unsupported intrinsic, never a generic crash.
                let msg = format!("{e}");
                assert!(
                    msg.contains("unsupported"),
                    "refusal must name the unsupported intrinsic; got: {msg}"
                );
            }
        }
    }
}
