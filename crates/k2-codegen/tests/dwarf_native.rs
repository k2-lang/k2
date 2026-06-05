//! v0.27 DWARF integration tests, gated on the DWARF *oracle* tools
//! (`llvm-dwarfdump`, `addr2line`, `readelf`) being present on `$PATH` and on an
//! x86-64 Linux host (the only place the emitted ELF executes).
//!
//! There is no gdb in this environment, so live source-level debugging is
//! validated indirectly: `llvm-dwarfdump --verify` parses every DWARF section and
//! cross-checks it, and `addr2line` maps a real in-function address back to the
//! right source line — the same DWARF gdb/lldb would consume. Each test below
//! `return`s cleanly (skips) when its tool or host is missing, exactly like the
//! existing `os_native.rs` host gate, so non-x86-64 / tool-less CI never breaks.
//!
//! What is asserted end-to-end for a real compiled program:
//!   * `readelf -S` shows `.debug_info` / `.debug_line` / `.debug_abbrev`.
//!   * `llvm-dwarfdump --verify` reports **no errors**.
//!   * `llvm-dwarfdump --debug-info` shows a `DW_TAG_compile_unit` + a
//!     `DW_TAG_subprogram` named `main` with correct `low_pc`/`high_pc`.
//!   * `llvm-dwarfdump --debug-line` maps addresses to the source file.
//!   * `addr2line` resolves an in-`main` address to the right `<src>:<line>` and
//!     reports the function name `main`.
//!   * The binary still RUNS and prints the expected output, and its loaded image
//!     is byte-identical to the no-DWARF build (DWARF changed nothing executed).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use k2_codegen::{compile_program_to_elf, compile_program_to_elf_with_debug, DebugCtx, Target};
use k2_mir::{lower_program, BuildMode, MirProgram};

/// `true` on an x86-64 Linux host, where the emitted ELF can execute and the
/// addresses in the DWARF match the running image.
fn can_run_native() -> bool {
    cfg!(target_arch = "x86_64") && cfg!(target_os = "linux")
}

/// Probes `$PATH` for `name`, returning its absolute path if a runnable copy is
/// found (its `--version` exits successfully). Mirrors the driver's `find_cc`.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            let ok = Command::new(&cand)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return Some(cand);
            }
        }
    }
    None
}

/// Lowers a self-contained k2 source string to a verified `MirProgram` (the std
/// prelude is injected), mirroring the `k2c` driver and `os_native.rs`.
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

/// A unique temp path under the system temp dir, tagged with the pid + a counter.
fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("k2_dwarf_{tag}_{}_{}", std::process::id(), n))
}

/// Writes `bytes` to `path` and `chmod 0o755`.
fn write_exe(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write temp binary");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// The hello-class program used across the tests: a `main` that prints two lines
/// (so the line table has several distinct source lines to resolve).
const HELLO: &str = r#"
pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("Hello, k2!\n", .{});
    const n: u32 = 42;
    try out.print("n = {d}\n", .{n});
}
"#;

/// Compiles `HELLO` to a `-g` ELF (DWARF v5) with the given source basename, runs
/// the requested oracle, and returns `(elf_bytes, dwarf_ctx)`.
fn build_hello_debug(src_name: &str) -> Vec<u8> {
    let prog = lower(HELLO);
    let ctx = DebugCtx {
        src_path: src_name.to_string(),
        comp_dir: "/k2/test".to_string(),
        ..DebugCtx::default()
    };
    compile_program_to_elf_with_debug(&prog, Target::X86_64Linux, &ctx)
        .expect("hello must be in the native subset")
        .bytes
}

#[test]
fn readelf_shows_debug_sections() {
    let Some(readelf) = tool("readelf") else {
        return;
    };
    let bytes = build_hello_debug("hello.k2");
    let p = temp_path("readelf");
    write_exe(&p, &bytes);
    let out = Command::new(&readelf).arg("-S").arg(&p).output().unwrap();
    let _ = std::fs::remove_file(&p);
    let s = String::from_utf8_lossy(&out.stdout);
    for want in [".debug_info", ".debug_line", ".debug_abbrev", ".debug_str"] {
        assert!(s.contains(want), "readelf -S must list {want}; got:\n{s}");
    }
}

#[test]
fn dwarfdump_verify_clean() {
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let bytes = build_hello_debug("hello.k2");
    let p = temp_path("verify");
    write_exe(&p, &bytes);
    let out = Command::new(&dd).arg("--verify").arg(&p).output().unwrap();
    let _ = std::fs::remove_file(&p);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && s.contains("No errors"),
        "llvm-dwarfdump --verify must report no errors; got status {:?}:\n{s}",
        out.status.code()
    );
}

#[test]
fn dwarfdump_shows_cu_and_main_subprogram() {
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let bytes = build_hello_debug("hello.k2");
    let p = temp_path("info");
    write_exe(&p, &bytes);
    let out = Command::new(&dd)
        .arg("--debug-info")
        .arg(&p)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("DW_TAG_compile_unit"), "CU DIE present:\n{s}");
    assert!(
        s.contains("DW_TAG_subprogram"),
        "subprogram DIE present:\n{s}"
    );
    assert!(
        s.contains("DW_AT_name") && s.contains("\"main\""),
        "main subprogram named:\n{s}"
    );
    assert!(s.contains("DW_AT_low_pc"), "low_pc present");
    assert!(s.contains("DW_AT_high_pc"), "high_pc present");
    // The CU low_pc is the text base 0x401000.
    assert!(
        s.contains("0x0000000000401000"),
        "CU low_pc = text base:\n{s}"
    );
}

#[test]
fn dwarfdump_line_table_maps_source() {
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let bytes = build_hello_debug("hello.k2");
    let p = temp_path("line");
    write_exe(&p, &bytes);
    let out = Command::new(&dd)
        .arg("--debug-line")
        .arg(&p)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("hello.k2"), "line table names the source:\n{s}");
    assert!(
        s.contains("end_sequence"),
        "line table has a terminated sequence:\n{s}"
    );
    // An address in the main text range appears in the table.
    assert!(s.contains("0x0000000000401"), "addresses present:\n{s}");
}

#[test]
fn addr2line_resolves_in_main_address() {
    if !can_run_native() {
        return;
    }
    // Prefer GNU addr2line (the acceptance tool); fall back to llvm-addr2line.
    let Some(a2l) = tool("addr2line").or_else(|| tool("llvm-addr2line")) else {
        return;
    };
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let bytes = build_hello_debug("hello.k2");
    let p = temp_path("a2l");
    write_exe(&p, &bytes);

    // Find main's low_pc from the DWARF, then resolve an address a few bytes in
    // (past the prologue) and confirm it lands on a real hello.k2 line.
    let info = Command::new(&dd)
        .arg("--debug-info")
        .arg(&p)
        .output()
        .unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    // The subprogram low_pc is the address line right after `DW_TAG_subprogram`.
    let low_pc = extract_subprogram_low_pc(&info).expect("main low_pc in dwarf");
    let probe = low_pc + 0x10; // a few bytes into main's body

    let out = Command::new(&a2l)
        .arg("-f")
        .arg("-e")
        .arg(&p)
        .arg(format!("{probe:#x}"))
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    // addr2line -f prints "<function>\n<file>:<line>". (GNU may also print a
    // harmless DWARF-v5 warning on stderr; we only read stdout.)
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("main"),
        "addr2line -f reports the function name main; got:\n{s}"
    );
    assert!(
        s.contains("hello.k2:"),
        "addr2line maps the address into hello.k2; got:\n{s}"
    );
}

#[test]
fn debug_binary_runs_and_matches_nodebug_image() {
    if !can_run_native() {
        return;
    }
    let prog = lower(HELLO);
    let ctx = DebugCtx {
        src_path: "hello.k2".to_string(),
        comp_dir: "/k2/test".to_string(),
        ..DebugCtx::default()
    };
    let g = compile_program_to_elf_with_debug(&prog, Target::X86_64Linux, &ctx)
        .unwrap()
        .bytes;
    let nog = compile_program_to_elf(&prog).unwrap().bytes;

    // The loaded image (everything before the section-header table) must be
    // identical except the four section-table Ehdr fields at offsets 0x28..0x40
    // (e_shoff/.../e_shstrndx) — all zero in the no-DWARF build. Compare the rest.
    let cmp_len = nog.len().min(g.len());
    for i in 0..cmp_len {
        // Skip the Ehdr section-table fields (0x28..=0x3f) and anything at/after
        // the no-DWARF length (which is the start of trailing metadata bounds).
        if (0x28..0x40).contains(&i) {
            continue;
        }
        assert_eq!(
            nog[i], g[i],
            "loaded byte {i:#x} differs between -g and no-g builds"
        );
    }

    // Both binaries run and print the same output.
    let run = |bytes: &[u8], tag: &str| -> (Vec<u8>, Option<i32>) {
        let p = temp_path(tag);
        write_exe(&p, bytes);
        let out = Command::new(&p).output().unwrap();
        let _ = std::fs::remove_file(&p);
        (out.stdout, out.status.code())
    };
    let (out_g, code_g) = run(&g, "rung");
    let (out_nog, code_nog) = run(&nog, "runnog");
    assert_eq!(out_g, out_nog, "stdout identical with and without DWARF");
    assert_eq!(
        code_g, code_nog,
        "exit code identical with and without DWARF"
    );
    assert_eq!(
        String::from_utf8_lossy(&out_g),
        "Hello, k2!\nn = 42\n",
        "expected hello output"
    );
}

// =========================================================================
//  Multi-file / file-aware DWARF (v0.27 fix): an `@import`-ed std function
//  inlined into a user program must resolve to ITS OWN file at a REAL line,
//  never to a nonexistent line of the user's (much shorter) main file.
// =========================================================================

/// A user program whose `test` blocks call `std.testing.expectEqual` /
/// `expectError`, so the std helpers (and their statements) are lowered into
/// `.text` with their own — far larger — `std.k2` line numbers. Without the
/// file-aware fix those addresses resolve to nonexistent lines of this short file.
const MULTI: &str = r#"const std = @import("std");

fn double(n: u32) u32 {
    return n * 2;
}

pub fn main(sys: *System) !void {
    const out = sys.io.stdout();
    try out.print("{d}\n", .{double(21)});
}

test "double doubles" {
    try std.testing.expectEqual(@as(u32, 84), double(42));
}
"#;

/// Builds the file-aware [`DwarfSourceMap`] for a self-contained test `source`,
/// mirroring the driver's `parse_program` layout (user source first, then the one
/// `const __k2_std_root = struct {` header line, then the `std` body). This is the
/// same construction `k2c` uses on the `-g` path, kept in lockstep here so the test
/// exercises the real mapping.
fn multi_source_map(source: &str) -> k2_codegen::DwarfSourceMap {
    let mut combined = String::from(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    let line_count_of = |s: &str| s.lines().count() as u32 + u32::from(!s.ends_with('\n'));
    let user_lines = line_count_of(&combined);
    let std_body_start = user_lines + 2; // +1 header line, +1 to reach the body
    let std_lines = line_count_of(k2_std::STD_BODY);
    k2_codegen::DwarfSourceMap::from_segments(vec![
        (1, user_lines, "multi.k2".to_string(), 1),
        (std_body_start, std_lines, "std.k2".to_string(), 1),
    ])
}

/// Parses every `(file_index_implied_path, line)` the `llvm-dwarfdump --debug-line`
/// table reports, joined with the file-name table, as `(file_name, line)` pairs.
/// Returns `None` if the dump cannot be parsed.
fn line_table_locations(dump: &str) -> Vec<(String, u32)> {
    // First collect the file-name table: lines like `file_names[  N]:` then a
    // following `name: "x"`.
    let mut files: Vec<String> = Vec::new();
    let mut lines = dump.lines().peekable();
    while let Some(l) = lines.next() {
        if l.trim_start().starts_with("file_names[") {
            // The next `name:` line holds the path.
            for n in lines.by_ref() {
                if let Some(rest) = n.trim().strip_prefix("name:") {
                    files.push(rest.trim().trim_matches('"').to_string());
                    break;
                }
            }
        }
    }
    // Then the row table: `Address Line Column File ...`. Rows start with `0x`.
    let mut out = Vec::new();
    for l in dump.lines() {
        let t = l.trim_start();
        if !t.starts_with("0x") {
            continue;
        }
        let cols: Vec<&str> = t.split_whitespace().collect();
        // Address Line Column File ISA ...
        if cols.len() >= 4 {
            if let (Ok(line), Ok(file_idx)) = (cols[1].parse::<u32>(), cols[3].parse::<usize>()) {
                let name = files.get(file_idx).cloned().unwrap_or_default();
                out.push((name, line));
            }
        }
    }
    out
}

#[test]
fn multi_file_no_nonexistent_main_line() {
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let prog = lower(MULTI);
    let user_lines = MULTI.lines().count() as u32;
    let ctx = DebugCtx {
        src_path: "multi.k2".to_string(),
        comp_dir: "/k2/test".to_string(),
        source_map: multi_source_map(MULTI),
    };
    let bytes = compile_program_to_elf_with_debug(&prog, Target::X86_64Linux, &ctx)
        .expect("multi must be in the native subset")
        .bytes;
    let p = temp_path("multi_line");
    write_exe(&p, &bytes);

    // Verify is clean.
    let verify = Command::new(&dd).arg("--verify").arg(&p).output().unwrap();
    let vs = String::from_utf8_lossy(&verify.stdout);
    assert!(
        verify.status.success() && vs.contains("No errors"),
        "llvm-dwarfdump --verify must report no errors; got:\n{vs}"
    );

    let out = Command::new(&dd)
        .arg("--debug-line")
        .arg(&p)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    let dump = String::from_utf8_lossy(&out.stdout);
    let locs = line_table_locations(&dump);
    assert!(!locs.is_empty(), "line table has rows:\n{dump}");

    // The core regression assertion: NO row attributed to the user file
    // (`multi.k2`) may reference a line beyond the user file's length. (The std
    // helper rows are attributed to `std.k2` and legitimately carry large lines.)
    for (file, line) in &locs {
        if file == "multi.k2" {
            assert!(
                *line <= user_lines,
                "user-file row points at multi.k2:{line}, beyond the file's \
                 {user_lines} lines (the v0.27 multi-file mis-attribution bug)"
            );
        }
    }
    // And the std helper's code IS present, attributed to std.k2 at a real (large)
    // line — proving the file-aware mapping actually fired, not merely that the
    // user rows are short.
    assert!(
        locs.iter()
            .any(|(f, line)| f == "std.k2" && *line > user_lines),
        "an inlined std row is attributed to std.k2 at its own (large) line:\n{dump}"
    );
}

#[test]
fn multi_file_addr2line_maps_std_to_std_file() {
    if !can_run_native() {
        return;
    }
    let Some(a2l) = tool("addr2line").or_else(|| tool("llvm-addr2line")) else {
        return;
    };
    let Some(dd) = tool("llvm-dwarfdump") else {
        return;
    };
    let prog = lower(MULTI);
    let ctx = DebugCtx {
        src_path: "multi.k2".to_string(),
        comp_dir: "/k2/test".to_string(),
        source_map: multi_source_map(MULTI),
    };
    let bytes = compile_program_to_elf_with_debug(&prog, Target::X86_64Linux, &ctx)
        .unwrap()
        .bytes;
    let p = temp_path("multi_a2l");
    write_exe(&p, &bytes);

    // Find the `expectEqual` subprogram's low_pc and probe a few bytes in.
    let info = Command::new(&dd)
        .arg("--debug-info")
        .arg(&p)
        .output()
        .unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    let probe = subprogram_low_pc_named(&info, "expectEqual").map(|lp| lp + 0x8);

    if let Some(probe) = probe {
        let out = Command::new(&a2l)
            .arg("-f")
            .arg("-e")
            .arg(&p)
            .arg(format!("{probe:#x}"))
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        // The address inside the inlined std helper resolves to std.k2 — NEVER to
        // multi.k2 (which has no such line).
        assert!(
            s.contains("std.k2:"),
            "an std-helper address resolves into std.k2; got:\n{s}"
        );
        assert!(
            !s.contains("multi.k2:"),
            "an std-helper address must NOT resolve to the user file; got:\n{s}"
        );
    }
    let _ = std::fs::remove_file(&p);
}

/// Finds the `DW_AT_low_pc` of the first `DW_TAG_subprogram` whose `DW_AT_name`
/// contains `needle`, from `llvm-dwarfdump --debug-info` text. The subprogram
/// abbrev emits `DW_AT_name` *before* `DW_AT_low_pc`, so we remember whether the
/// current subprogram's name matched and capture its low_pc.
fn subprogram_low_pc_named(dump: &str, needle: &str) -> Option<u64> {
    let mut name_matched = false;
    let mut in_sub = false;
    for line in dump.lines() {
        if line.contains("DW_TAG_subprogram") {
            in_sub = true;
            name_matched = false;
        } else if in_sub && line.contains("DW_AT_name") {
            name_matched = line.contains(needle);
        } else if in_sub && name_matched && line.contains("DW_AT_low_pc") {
            let start = line.find("0x")?;
            let hex = &line[start + 2..];
            let end = hex
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(hex.len());
            return u64::from_str_radix(&hex[..end], 16).ok();
        }
    }
    None
}

/// Extracts the first `DW_TAG_subprogram`'s `DW_AT_low_pc` hex value from
/// `llvm-dwarfdump --debug-info` text. Returns `None` if not found.
fn extract_subprogram_low_pc(dump: &str) -> Option<u64> {
    let mut in_sub = false;
    for line in dump.lines() {
        if line.contains("DW_TAG_subprogram") {
            in_sub = true;
        } else if in_sub && line.contains("DW_AT_low_pc") {
            // e.g. `DW_AT_low_pc\t(0x0000000000401014)`
            let start = line.find("0x")?;
            let hex = &line[start + 2..];
            let end = hex
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(hex.len());
            return u64::from_str_radix(&hex[..end], 16).ok();
        }
    }
    None
}
