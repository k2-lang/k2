//! End-to-end execution tests for the v0.23 OS-effect capabilities: `sys.fs`
//! (file write/read/stat round-trip), `sys.os` (argv/env/pid), `sys.time` (real
//! monotonic clock + sleep + `Duration`/`Instant`), and `sys.net` (a loopback TCP
//! echo). Every test is DETERMINISTIC, OFFLINE, and self-cleaning:
//!
//! * filesystem tests use a UNIQUE temp path (process id + a per-test counter,
//!   injected as argv so the k2 program owns the exact path) and delete the file
//!   themselves; a Rust teardown guard removes it too, even on a failed assert;
//! * the env tests use a SCRIPTED env map (never the host env), so the value is
//!   fixed; `getpid` is asserted only `> 0`, never an exact value;
//! * the time tests assert only INEQUALITIES (monotonic increased, slept ≥ a loose
//!   lower bound), never exact nanoseconds, so they are robust to scheduler jitter;
//! * the net test binds an EPHEMERAL loopback port (`127.0.0.1:0`, never asserted),
//!   so parallel `cargo test` runs never collide and no external network is touched.
//!
//! All effects flow through a capability passed from `*System` (no ambient global);
//! the VM backs them with Rust `std`.

use std::sync::atomic::{AtomicU32, Ordering};

use k2_mir::{lower_program, BuildMode};
use k2_parse::{parse, ParseResult};
use k2_resolve::resolve_file;
use k2_syntax::{Expr, Item, SourceFile};
use k2_types::check_file;
use k2_vm::{run_captured, OsInputs, RunArgs, RunOutcome};

// ---- std-injecting front-end harness (mirrors the CLI's `parse_program`) ----

/// Parses `source` together with the bundled std, re-pointing every
/// `const X = @import("std")` at the synthetic std root so `std.fs.*`/`std.time.*`
/// are real compiled declarations.
fn parse_with_std(source: &str) -> ParseResult {
    let mut combined = String::with_capacity(source.len() + k2_std::STD_BODY.len() + 64);
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());
    let mut result = parse(&combined);
    rewrite_std_imports(&mut result.file);
    result
}

/// Re-points every `const X = @import("std")` to the synthetic std root.
fn rewrite_std_imports(file: &mut SourceFile) {
    for item in &mut file.items {
        if let Item::Const { value, .. } = item {
            if import_target(value).as_deref() == Some("std") {
                let span = value.span();
                *value = Expr::Ident {
                    name: k2_std::STD_ROOT_NAME.to_string(),
                    span,
                };
            }
        }
    }
}

/// If `e` is exactly `@import("name")`, returns the imported name.
fn import_target(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                return Some(text.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Lowers a source string (with the std prelude injected) to a verified
/// `MirProgram`, asserting the front-end stages are clean.
fn lower(src: &str) -> k2_mir::MirProgram {
    let pres = parse_with_std(src);
    assert!(pres.is_ok(), "parse errors: {:?}", pres.diagnostics);
    let resolved = resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve errors: {:?}",
        resolved.diagnostics
    );
    let typed = check_file(&pres.file, &resolved);
    assert!(typed.is_ok(), "type errors: {:?}", typed.diagnostics);
    let prog =
        lower_program(&pres.file, &resolved, typed, BuildMode::Debug).expect("lowering succeeds");
    let problems = prog.verify();
    assert!(problems.is_empty(), "malformed MIR: {problems:?}");
    prog
}

/// Runs `src` with the given OS inputs (argv / scripted env), returning
/// `(stdout, stderr, outcome, exit)`.
fn run_with(src: &str, os: OsInputs) -> (String, String, RunOutcome, i32) {
    let prog = lower(src);
    let args = RunArgs {
        mode: BuildMode::Debug,
        argv: Vec::new(),
        os,
        trace_label: None,
    };
    let (outcome, code, out, err) = run_captured(&prog, args);
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
        outcome,
        code,
    )
}

/// A process-unique temp path for a filesystem test. The pid keeps it distinct
/// across parallel `cargo test` invocations; the per-call counter keeps each test
/// distinct within this process. The file is removed by the k2 program itself AND
/// by a [`TempFile`] teardown guard.
fn unique_temp_path(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}/k2_v23_{}_{}_{}.tmp",
        std::env::temp_dir().display(),
        tag,
        std::process::id(),
        n
    )
}

/// A teardown guard that removes a temp file when dropped — so a failed assertion
/// never leaves a stray file behind (the k2 program also deletes it on success).
struct TempFile(String);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// =========================================================================
//  T1 — fs write / read round-trip + stat (VM)
// =========================================================================

#[test]
fn fs_write_read_roundtrip_identical_and_stat() {
    let path = unique_temp_path("roundtrip");
    let _guard = TempFile(path.clone());
    // The program reads its temp path from argv[0] so the Rust test owns uniqueness.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);
    const payload = "k2 v0.23 fs round-trip payload";

    var wf = try sys.fs.create(path);
    const nw = try wf.write(payload);
    wf.close();

    const st = try sys.fs.stat(path);

    var rf = try sys.fs.openRead(path);
    var buf: [128]u8 = undefined;
    const nr = try rf.read(&buf);
    rf.close();

    const same = std.mem.eql(u8, payload, buf[0..nr]);
    try out.print("wrote={d} read={d} size={d} match={} content={s}\n", .{ nw, nr, st.size, same, buf[0..nr] });

    try sys.fs.delete(path);
    try out.print("exists_after_delete={}\n", .{sys.fs.exists(path)});
}
"#;
    let os = OsInputs {
        argv: vec![path.clone()],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "wrote=30 read=30 size=30 match=true content=k2 v0.23 fs round-trip payload\n\
         exists_after_delete=false\n"
    );
    // The program self-cleaned; confirm the file is gone (the guard is a backstop).
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn fs_make_and_remove_dir() {
    let dir = unique_temp_path("dir");
    let _guard = TempFile(dir.clone());
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const dir = sys.os.arg(0);
    try sys.fs.makeDir(dir);
    const st = try sys.fs.stat(dir);
    try out.print("made is_dir={} exists={}\n", .{ st.is_dir, sys.fs.exists(dir) });
    try sys.fs.removeDir(dir);
    try out.print("removed exists={}\n", .{sys.fs.exists(dir)});
}
"#;
    let os = OsInputs {
        argv: vec![dir.clone()],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "made is_dir=true exists=true\nremoved exists=false\n");
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn fs_open_missing_file_is_clean_error() {
    // Reading a path that does not exist returns `error.FileNotFound` (a clean k2
    // error value, never a host panic). Self-contained: the path is never created.
    let path = unique_temp_path("missing");
    let _guard = TempFile(path.clone());
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);
    const r = sys.fs.openRead(path);
    if (r) |f| {
        var ff = f;
        ff.close();
        try out.print("unexpectedly opened\n", .{});
    } else |e| {
        try out.print("err={s}\n", .{@errorName(e)});
    }
}
"#;
    let os = OsInputs {
        argv: vec![path],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "err=FileNotFound\n");
}

// =========================================================================
//  T2 — argv + scripted env + getpid (VM)
// =========================================================================

#[test]
fn os_argv_and_pid() {
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("argc={d} arg0={s} arg1={s} pid_ok={}\n", .{
        sys.os.argCount(), sys.os.arg(0), sys.os.arg(1), sys.os.getpid() > 0,
    });
}
"#;
    let os = OsInputs {
        argv: vec!["alpha".into(), "beta".into()],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "argc=2 arg0=alpha arg1=beta pid_ok=true\n");
}

#[test]
fn os_env_scripted_present_and_absent() {
    // The env value is SCRIPTED (never the host env), so the output is fixed.
    // `orelse` is the idiomatic optional-default form for an env lookup.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const present = sys.env.get("K2_TEST_VAR") orelse "<absent>";
    const absent = sys.env.get("K2_NOT_SET") orelse "<absent>";
    try out.print("present={s} absent={s}\n", .{ present, absent });
}
"#;
    let os = OsInputs {
        env: vec![("K2_TEST_VAR".into(), "hello-v23".into())],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "present=hello-v23 absent=<absent>\n");
}

#[test]
fn os_env_default_is_offline_absent() {
    // With no scripted vars and no host opt-in, EVERY lookup is absent — the
    // reproducible default that keeps the whole corpus deterministic.
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const v = sys.env.get("PATH") orelse "<absent>";
    try out.print("path={s}\n", .{v});
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "path=<absent>\n");
}

// =========================================================================
//  T3 — real monotonic time + sleep + Duration/Instant (VM)
// =========================================================================

#[test]
fn time_monotonic_increases_and_sleep_delays() {
    // Asserts only INEQUALITIES / loose lower bounds, never exact nanos: monotonic
    // is non-decreasing, and a 5 ms sleep advances it by at least 1 ms (a generous
    // tolerance robust to scheduler jitter). A `Duration` from `elapsedSince` is
    // also at least 1 ms. No exact timing is asserted, so this is never flaky.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const t0 = sys.time.monotonicReal();
    sys.time.sleepReal(5_000_000);
    const t1 = sys.time.monotonicReal();

    const start = std.time.Instant.fromNanos(sys.time.monotonicReal());
    sys.time.sleepReal(5_000_000);
    const d = start.elapsedSince(sys.time.monotonicReal());

    try out.print("mono_inc={} slept_at_least_1ms={} elapsed_at_least_1ms={} wall_positive={}\n", .{
        t1 >= t0,
        (t1 - t0) >= 1_000_000,
        d.asMillis() >= 1,
        sys.time.nowReal() > 0,
    });
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "mono_inc=true slept_at_least_1ms=true elapsed_at_least_1ms=true wall_positive=true\n"
    );
}

#[test]
fn time_deterministic_clock_unchanged() {
    // The DETERMINISTIC clock (`sys.clock`) is the default and stays byte-exact: it
    // starts at 0 and only advances on `sleep`. This is the regression guard that
    // real time is purely additive and never perturbs the reproducible path.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const a = sys.clock.monotonicNanos();
    sys.clock.sleep(250);
    const b = sys.clock.monotonicNanos();
    const fake = std.time.Duration.fromMillis(3);
    try out.print("a={d} b={d} dur_ns={d} dur_ms={d}\n", .{ a, b, fake.ns, fake.asMillis() });
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "a=0 b=250 dur_ns=3000000 dur_ms=3\n");
}

// =========================================================================
//  T4 — loopback TCP echo (VM)
// =========================================================================

#[test]
fn net_loopback_echo_roundtrips_bytes() {
    // A single-fiber loopback echo: bind an EPHEMERAL port (never asserted), connect,
    // accept, the client sends, the server echoes, the client receives the IDENTICAL
    // bytes. Loopback only; sockets close on scope exit (self-cleaning). The small
    // payload fits the kernel buffers so the single-fiber sequence never blocks.
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();

    var listener = try sys.net.listen(0);
    const port = listener.localPort();

    var client = try sys.net.connect("127.0.0.1", port);
    var server = try listener.accept();

    const msg = "ping-v23-echo";
    const ns = try client.send(msg);

    var sbuf: [64]u8 = undefined;
    const nr = try server.recv(&sbuf);
    const ne = try server.send(sbuf[0..nr]);

    var cbuf: [64]u8 = undefined;
    const nc = try client.recv(&cbuf);
    const same = std.mem.eql(u8, msg, cbuf[0..nc]);

    try out.print("sent={d} echoed={d} recv={d} match={} port_nonzero={} content={s}\n", .{
        ns, ne, nc, same, port > 0, cbuf[0..nc],
    });

    client.close();
    server.close();
    listener.close();
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "sent=13 echoed=13 recv=13 match=true port_nonzero=true content=ping-v23-echo\n"
    );
}

// =========================================================================
//  T5 — capability passing (no ambient global)
// =========================================================================

#[test]
fn capabilities_thread_from_system_no_global() {
    // Every effect is reached only through `*System`: `main` opens the file via the
    // `sys.fs` door, then threads the resulting `File` HANDLE into a helper that
    // writes through it. The helper has no `*System` and no `sys.fs` — its only
    // authority is the handle it was handed, exactly the capability discipline (a
    // function that was never given a handle literally cannot touch the file).
    let path = unique_temp_path("cap");
    let _guard = TempFile(path.clone());
    let src = r#"
fn writeThrough(file: anytype, bytes: []const u8) !usize {
    return file.write(bytes);
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);
    var f = try sys.fs.create(path);
    const n = try writeThrough(&f, "cap-threaded");
    f.close();
    try out.print("wrote={d}\n", .{n});
    try sys.fs.delete(path);
}
"#;
    let os = OsInputs {
        argv: vec![path],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "wrote=12\n");
}

#[test]
fn fs_capability_passed_by_value() {
    // The `Fs` capability itself threads by value: a helper that takes `sys.fs`
    // can open and write a file; a helper without it could not. This is the
    // "capabilities compose" story — authority is a value you hand down explicitly.
    let path = unique_temp_path("capval");
    let _guard = TempFile(path.clone());
    let src = r#"
fn writeFile(fsc: anytype, path: []const u8, bytes: []const u8) !usize {
    var f = try fsc.create(path);
    const n = try f.write(bytes);
    f.close();
    return n;
}
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);
    const n = try writeFile(sys.fs, path, "by-value-cap");
    const st = try sys.fs.stat(path);
    try out.print("wrote={d} size={d}\n", .{ n, st.size });
    try sys.fs.delete(path);
}
"#;
    let os = OsInputs {
        argv: vec![path],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "wrote=12 size=12\n");
}

// =========================================================================
//  T6 — v0.23 regression suite (the six verified-defect fixes)
// =========================================================================

/// Runs `src` against a `Vm` directly (so the test can inspect VM-internal state),
/// returning `(stdout, the Vm)`. Asserts the run completed cleanly.
fn run_on_vm(src: &str, os: OsInputs) -> (String, k2_vm::Vm<'static>) {
    let prog = Box::leak(Box::new(lower(src)));
    let mut vm = k2_vm::Vm::new(prog);
    vm.with_os_inputs(os);
    let res = vm.run_main();
    assert!(res.is_ok(), "run failed: {res:?}");
    (String::from_utf8_lossy(vm.stdout()).into_owned(), vm)
}

/// **REGRESSION (finding #1/#4)**: reading a file back into a slice of an
/// `= undefined` stack array (`var buf:[N]u8=undefined; rf.read(buf[0..N])`) must
/// deliver the real bytes BYTE-FOR-BYTE. Before the slice-of-undefined-array
/// aliasing fix, the slice's `ptr` did not alias `buf`, so `read` reported a
/// (false) count of N while every `buf[j]` stayed zero. The program asserts the
/// read-back bytes equal the known payload via index, so a silent zero-fill fails.
#[test]
fn fs_read_into_undefined_stack_array_slice_is_byte_exact() {
    let path = unique_temp_path("read_slice");
    let _guard = TempFile(path.clone());
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);

    // Write known, distinct bytes (100..107) so a zero-fill is unmistakable.
    var wf = try sys.fs.create(path);
    var src_bytes: [8]u8 = undefined;
    var i: usize = 0;
    while (i < 8) : (i += 1) { src_bytes[i] = @intCast(100 + i); }
    const nw = try wf.write(src_bytes[0..8]);
    wf.close();

    // Read back into a slice of an `= undefined` stack array (the idiomatic form).
    var rf = try sys.fs.openRead(path);
    var buf: [8]u8 = undefined;
    const nr = try rf.read(buf[0..8]);
    rf.close();

    var ok = true;
    var j: usize = 0;
    while (j < nr) : (j += 1) {
        if (buf[j] != @as(u8, @intCast(100 + j))) { ok = false; }
    }
    try out.print("nw={d} nr={d} exact={}\n", .{ nw, nr, ok });
    try sys.fs.delete(path);
}
"#;
    let os = OsInputs {
        argv: vec![path],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "nw=8 nr=8 exact=true\n");
}

/// **REGRESSION (finding #2/#3)**: `write(&array)` (a whole-array `*[N]u8`
/// pointer) and `write(slice)` must BOTH write `array.len()` bytes, and the file
/// must read back identically — with NO destructive truncation. Before the fix,
/// the byte-source extractor skipped `Value::Ptr`, so `write(&payload)` sourced 0
/// bytes and (O_TRUNC) zeroed the file while falsely reporting success.
#[test]
fn fs_write_via_array_pointer_and_slice_no_truncation() {
    let path = unique_temp_path("write_ptr");
    let _guard = TempFile(path.clone());
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);

    var payload: [6]u8 = .{ 10, 20, 30, 40, 50, 60 };

    // Form A: write the whole-array pointer `&payload` (a `*[6]u8`).
    var f1 = try sys.fs.create(path);
    const nw1 = try f1.write(&payload);
    f1.close();
    const st1 = try sys.fs.stat(path);

    // Read it back and compare byte-for-byte.
    var r1 = try sys.fs.openRead(path);
    var rb: [6]u8 = undefined;
    const nr1 = try r1.read(rb[0..6]);
    r1.close();
    const same1 = std.mem.eql(u8, payload[0..6], rb[0..nr1]);

    // Form B: re-create (O_TRUNC) and write the SLICE form; must NOT lose data.
    var f2 = try sys.fs.create(path);
    const nw2 = try f2.write(payload[0..6]);
    f2.close();
    const st2 = try sys.fs.stat(path);

    try out.print("ptr_nw={d} ptr_size={d} ptr_same={} slice_nw={d} slice_size={d}\n", .{
        nw1, st1.size, same1, nw2, st2.size,
    });
    try sys.fs.delete(path);
}
"#;
    let os = OsInputs {
        argv: vec![path],
        ..OsInputs::default()
    };
    let (out, err, outcome, code) = run_with(src, os);
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "ptr_nw=6 ptr_size=6 ptr_same=true slice_nw=6 slice_size=6\n"
    );
}

/// **REGRESSION (finding #2/#3, net side)**: `send(&array)` (a whole-array
/// pointer) must transmit `array.len()` bytes over a loopback socket — the same
/// `Value::Ptr` byte-source defect that silently dropped `fs.write(&array)` also
/// made `net.send(&array)` send 0 bytes and then DEADLOCK the blocking `recv`.
#[test]
fn net_send_via_array_pointer_transmits_all_bytes() {
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var listener = try sys.net.listen(0);
    const port = listener.localPort();
    var client = try sys.net.connect("127.0.0.1", port);
    var server = try listener.accept();

    var msg: [5]u8 = .{ 7, 8, 9, 10, 11 };
    const ns = try client.send(&msg); // whole-array pointer source

    var sbuf: [16]u8 = undefined;
    const nr = try server.recv(sbuf[0..16]); // slice destination
    var same = (ns == nr);
    var i: usize = 0;
    while (i < nr) : (i += 1) {
        if (sbuf[i] != msg[i]) { same = false; }
    }
    try out.print("sent={d} recv={d} same={}\n", .{ ns, nr, same });

    client.close();
    server.close();
    listener.close();
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(out, "sent=5 recv=5 same=true\n");
}

/// **REGRESSION (finding #4, net)**: a loopback echo whose recv destination is a
/// slice of an `= undefined` stack array (`server.recv(sbuf[0..n])`) must observe
/// the exact bytes — the slice-of-undefined-array aliasing fix on the recv path.
#[test]
fn net_recv_into_undefined_stack_array_slice_is_byte_exact() {
    let src = r#"
const std = @import("std");
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    var listener = try sys.net.listen(0);
    const port = listener.localPort();
    var client = try sys.net.connect("127.0.0.1", port);
    var server = try listener.accept();

    const msg = "echo-into-slice";
    const ns = try client.send(msg);

    var sbuf: [64]u8 = undefined;
    const nr = try server.recv(sbuf[0..64]);
    const ne = try server.send(sbuf[0..nr]); // echo back the received slice

    var cbuf: [64]u8 = undefined;
    const nc = try client.recv(cbuf[0..64]);
    const same = std.mem.eql(u8, msg, cbuf[0..nc]);
    try out.print("sent={d} echoed={d} recv={d} match={} content={s}\n", .{ ns, ne, nc, same, cbuf[0..nc] });

    client.close();
    server.close();
    listener.close();
}
"#;
    let (out, err, outcome, code) = run_with(src, OsInputs::default());
    assert_eq!(outcome, RunOutcome::Ok, "stderr: {err}");
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "sent=15 echoed=15 recv=15 match=true content=echo-into-slice\n"
    );
}

/// **REGRESSION (finding #5)**: opening and closing many files in a loop must
/// REUSE vacated handle-table slots, so the table stays bounded (it does not grow
/// one slot per open). With at most one file open at a time, `insert_handle`
/// always reuses slot 0, so the `files` table length stays `1` after 64 iterations
/// — proven by the VM-internal accessor (the run is also clean, no fd exhaustion).
#[test]
fn fs_open_close_loop_reuses_handle_slots() {
    let path = unique_temp_path("reuse");
    let _guard = TempFile(path.clone());
    let src = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    const path = sys.os.arg(0);
    var k: usize = 0;
    while (k < 64) : (k += 1) {
        var f = try sys.fs.create(path);
        _ = try f.write("x");
        f.close();
    }
    try sys.fs.delete(path);
    try out.print("done\n", .{});
}
"#;
    let os = OsInputs {
        argv: vec![path.clone()],
        ..OsInputs::default()
    };
    let (out, vm) = run_on_vm(src, os);
    assert_eq!(out, "done\n");
    let (files, _listeners, _streams) = vm.os_handle_table_sizes();
    // 64 opens, each closed before the next: the table never exceeds one slot.
    assert_eq!(
        files, 1,
        "open/close loop must reuse the vacated slot (table stayed bounded), got {files}"
    );
    assert!(!std::path::Path::new(&path).exists());
}
