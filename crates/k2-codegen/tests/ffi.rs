//! v0.19 C-interop / FFI integration tests.
//!
//! These exercise the relocatable-object emission path end to end: a k2 program
//! that calls a libc function compiles to an `ET_REL` object, links via the
//! system `cc` (`-no-pie`), and RUNS with the expected output; a k2 `export fn`
//! is callable from a gcc-compiled C harness; and `@sizeOf`/`@alignOf` of C
//! structs match the C ABI (verified by compiling + running a C comparison).
//!
//! Every test that shells out to `cc` begins with a `find_cc()` probe and returns
//! early (skipping cleanly) when no C toolchain is present, so CI without a C
//! compiler still passes. The link/run tests additionally require an x86-64 Linux
//! host (the object is x86-64; running it needs an x86-64 kernel).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use k2_codegen::compile_program_to_object;
use k2_mir::{lower_program, BuildMode, MirProgram};

/// Lowers a self-contained k2 source string to a verified `MirProgram` through
/// the real front-end (parse -> resolve -> check -> lower -> verify), mirroring
/// `k2c`. Panics loudly with diagnostics on any front-end error.
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
    assert!(
        typed.is_ok(),
        "type errors: {:?}",
        typed.diagnostics.iter().filter(|d| d.is_error()).count()
    );
    let prog =
        lower_program(&pres.file, &resolved, typed, BuildMode::Debug).expect("lowering failed");
    assert!(prog.is_ok(), "lowering diagnostics in test program");
    let problems = prog.verify();
    assert!(problems.is_empty(), "malformed MIR: {problems:?}");
    prog
}

/// Re-points `const X = @import("std")` to the synthetic std root (the same
/// rewrite the CLI driver performs), so the `*System` capability methods resolve.
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

/// Probes for a usable C compiler (`$CC`, then `cc`, then `gcc`). Returns the
/// first whose `--version` runs, or `None` so a test can skip cleanly.
fn find_cc() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(cc) = std::env::var("CC") {
        if !cc.is_empty() {
            candidates.push(cc);
        }
    }
    candidates.push("cc".to_string());
    candidates.push("gcc".to_string());
    for cand in candidates {
        let ok = Command::new(&cand)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(cand);
        }
    }
    None
}

/// A unique temp path keyed by pid + an atomic counter, so parallel tests never
/// collide on the same inode.
fn temp_path(suffix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("k2-ffi-{}-{}-{}", std::process::id(), n, suffix))
}

/// Reads a little-endian `u16`/`u32`/`u64` from `bytes` at `off`.
fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn rd64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

// =========================================================================
//  T4 — object-writer structural unit tests (no `cc` needed)
// =========================================================================

/// A minimal puts-calling program emits a structurally well-formed `ET_REL`
/// object: `e_type=ET_REL`, `e_machine=EM_X86_64`, a UND `puts` symbol, a defined
/// GLOBAL `main`, a `R_X86_64_PLT32` reloc at the call site, and a `.rodata`
/// reference relocation. Parsed back from the raw bytes — no `cc` required.
#[test]
fn object_is_structurally_well_formed() {
    let prog = lower(
        r#"
        extern fn puts(s: [*:0]const u8) c_int;
        pub fn main() c_int { _ = puts("hi"); return 0; }
        "#,
    );
    let obj = compile_program_to_object(&prog, k2_codegen::Target::X86_64Linux)
        .expect("object emission failed");
    let b = &obj.bytes;

    // ELF magic + class + ET_REL + EM_X86_64.
    assert_eq!(&b[0..4], &[0x7F, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(b[4], 2, "ELFCLASS64");
    assert_eq!(rd16(b, 16), 1, "e_type = ET_REL");
    assert_eq!(rd16(b, 18), 0x3E, "e_machine = EM_X86_64");
    assert!(obj.had_extern, "puts is an undefined extern");
    assert!(
        obj.exports.iter().any(|e| e == "main"),
        "main is exported as the C entry"
    );

    // Walk the section headers to find .symtab / .strtab / .rela.text.
    let shoff = rd64(b, 40) as usize;
    let shentsize = rd16(b, 58) as usize;
    let shnum = rd16(b, 60) as usize;
    let shstrndx = rd16(b, 62) as usize;
    let shstr_off = rd64(b, shoff + shstrndx * shentsize + 24) as usize;
    let sec_name = |name_off: u32| -> String {
        let start = shstr_off + name_off as usize;
        let end = b[start..].iter().position(|&c| c == 0).unwrap() + start;
        String::from_utf8_lossy(&b[start..end]).to_string()
    };

    let mut symtab = None;
    let mut strtab = None;
    let mut rela = None;
    let mut names = Vec::new();
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let name = sec_name(rd32(b, base));
        names.push(name.clone());
        let off = rd64(b, base + 24) as usize;
        let size = rd64(b, base + 32) as usize;
        let link = rd32(b, base + 40);
        let info = rd32(b, base + 44);
        match name.as_str() {
            ".symtab" => symtab = Some((off, size, link, info)),
            ".strtab" => strtab = Some((off, size)),
            ".rela.text" => rela = Some((off, size, link, info)),
            _ => {}
        }
    }
    for want in [
        ".text",
        ".rodata",
        ".symtab",
        ".strtab",
        ".rela.text",
        ".shstrtab",
    ] {
        assert!(names.iter().any(|n| n == want), "missing section {want}");
    }

    // Parse the symbol table; find a UND `puts` and a defined GLOBAL `main`.
    let (symoff, symsize, symlink, syminfo) = symtab.expect(".symtab present");
    let (stroff, _strsize) = strtab.expect(".strtab present");
    let sym_name = |name_off: u32| -> String {
        let start = stroff + name_off as usize;
        let end = b[start..].iter().position(|&c| c == 0).unwrap() + start;
        String::from_utf8_lossy(&b[start..end]).to_string()
    };
    assert_eq!(symlink as usize, {
        // sh_link of .symtab is the .strtab section index.
        let mut idx = 0;
        for i in 0..shnum {
            if sec_name(rd32(b, shoff + i * shentsize)) == ".strtab" {
                idx = i;
            }
        }
        idx
    });
    let nsyms = symsize / 24;
    let mut puts_idx = None;
    let mut main_idx = None;
    let mut first_global_seen = None;
    for i in 0..nsyms {
        let base = symoff + i * 24;
        let name = sym_name(rd32(b, base));
        let info = b[base + 4];
        let bind = info >> 4;
        let shndx = rd16(b, base + 6);
        if bind == 1 && first_global_seen.is_none() {
            first_global_seen = Some(i);
        }
        if name == "puts" {
            assert_eq!(shndx, 0, "puts is UND (st_shndx == SHN_UNDEF)");
            assert_eq!(bind, 1, "puts is GLOBAL");
            puts_idx = Some(i);
        }
        if name == "main" {
            assert_ne!(shndx, 0, "main is defined (st_shndx != UND)");
            assert_eq!(bind, 1, "main is GLOBAL");
            assert_eq!(info & 0xf, 2, "main is STT_FUNC");
            main_idx = Some(i);
        }
    }
    assert!(puts_idx.is_some(), "puts symbol present");
    assert!(main_idx.is_some(), "main symbol present");
    // sh_info of .symtab is the index of the first global symbol; all locals must
    // precede it.
    assert_eq!(
        syminfo as usize,
        first_global_seen.expect("a global symbol exists"),
        ".symtab sh_info == first global index"
    );

    // The relocations: a PLT32 against `puts` (addend -4) and a `.rodata` ref.
    let (reloff, relsize, _rellink, relinfo) = rela.expect(".rela.text present");
    // sh_info of .rela.text is the .text section index (1 in our layout).
    assert_eq!(relinfo, 1, ".rela.text targets .text (section 1)");
    let nrel = relsize / 24;
    let mut saw_plt32_puts = false;
    let mut saw_data_reloc = false;
    for i in 0..nrel {
        let base = reloff + i * 24;
        let info = rd64(b, base + 8);
        let typ = (info & 0xffff_ffff) as u32;
        let sym = (info >> 32) as u32;
        let addend = rd64(b, base + 16) as i64;
        if typ == 4 {
            // R_X86_64_PLT32
            if Some(sym as usize) == puts_idx {
                assert_eq!(addend, -4, "PLT32 addend is -4");
                saw_plt32_puts = true;
            }
        }
        if typ == 1 {
            // R_X86_64_64 — the `.rodata` string pointer.
            saw_data_reloc = true;
        }
    }
    assert!(saw_plt32_puts, "a R_X86_64_PLT32 reloc against puts");
    assert!(saw_data_reloc, "a R_X86_64_64 reloc for the .rodata string");
}

/// An `export fn` produces a defined GLOBAL `STT_FUNC` symbol under its
/// un-mangled C name (no `extern` undefined symbols, since it calls nothing).
#[test]
fn export_fn_is_a_defined_global_symbol() {
    let prog = lower("export fn k2_add(a: c_int, b: c_int) c_int { return a + b; }");
    let obj = compile_program_to_object(&prog, k2_codegen::Target::X86_64Linux)
        .expect("object emission failed");
    assert!(!obj.had_extern, "no extern symbols");
    assert!(obj.exports.iter().any(|e| e == "k2_add"), "k2_add exported");

    let b = &obj.bytes;
    let shoff = rd64(b, 40) as usize;
    let shentsize = rd16(b, 58) as usize;
    let shnum = rd16(b, 60) as usize;
    let shstrndx = rd16(b, 62) as usize;
    let shstr_off = rd64(b, shoff + shstrndx * shentsize + 24) as usize;
    let sec_name = |name_off: u32| -> String {
        let start = shstr_off + name_off as usize;
        let end = b[start..].iter().position(|&c| c == 0).unwrap() + start;
        String::from_utf8_lossy(&b[start..end]).to_string()
    };
    let mut symtab = None;
    let mut strtab = None;
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let name = sec_name(rd32(b, base));
        let off = rd64(b, base + 24) as usize;
        let size = rd64(b, base + 32) as usize;
        match name.as_str() {
            ".symtab" => symtab = Some((off, size)),
            ".strtab" => strtab = Some(off),
            _ => {}
        }
    }
    let (symoff, symsize) = symtab.unwrap();
    let stroff = strtab.unwrap();
    let nsyms = symsize / 24;
    let mut found = false;
    for i in 0..nsyms {
        let base = symoff + i * 24;
        let nameoff = rd32(b, base);
        let start = stroff + nameoff as usize;
        let end = b[start..].iter().position(|&c| c == 0).unwrap() + start;
        let name = String::from_utf8_lossy(&b[start..end]);
        if name == "k2_add" {
            let info = b[base + 4];
            assert_eq!(info >> 4, 1, "k2_add is GLOBAL");
            assert_eq!(info & 0xf, 2, "k2_add is STT_FUNC");
            assert_ne!(rd16(b, base + 6), 0, "k2_add is defined (not UND)");
            found = true;
        }
    }
    assert!(found, "k2_add symbol present");
}

// =========================================================================
//  T1 — k2 calls libc (compile -> object -> cc-link -> RUN)
// =========================================================================

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn k2_calls_libc_puts_runs() {
    let Some(cc) = find_cc() else {
        eprintln!("skipping: no C compiler found");
        return;
    };
    let prog = lower(
        r#"
        extern fn puts(s: [*:0]const u8) c_int;
        pub fn main() c_int { _ = puts("hello, ffi"); return 0; }
        "#,
    );
    let obj = compile_program_to_object(&prog, k2_codegen::Target::X86_64Linux).unwrap();
    let (code, out) = link_and_run(&cc, &obj.bytes, &[]);
    assert_eq!(code, 0, "exit code 0");
    assert_eq!(out, b"hello, ffi\n", "puts output");
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn k2_calls_libc_printf_variadic_runs() {
    let Some(cc) = find_cc() else {
        eprintln!("skipping: no C compiler found");
        return;
    };
    let prog = lower(
        r#"
        extern fn printf(fmt: [*:0]const u8, ...) c_int;
        pub fn main() c_int { _ = printf("n=%d\n", 42); return 0; }
        "#,
    );
    let obj = compile_program_to_object(&prog, k2_codegen::Target::X86_64Linux).unwrap();
    let (code, out) = link_and_run(&cc, &obj.bytes, &[]);
    assert_eq!(code, 0, "exit code 0");
    assert_eq!(out, b"n=42\n", "printf output (variadic AL path)");
}

// =========================================================================
//  T2 — C calls a k2 export fn (gcc harness + RUN)
// =========================================================================

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn c_calls_k2_export_fn_runs() {
    let Some(cc) = find_cc() else {
        eprintln!("skipping: no C compiler found");
        return;
    };
    let prog = lower("export fn k2_add(a: c_int, b: c_int) c_int { return a + b; }");
    let obj = compile_program_to_object(&prog, k2_codegen::Target::X86_64Linux).unwrap();

    let obj_path = temp_path("k2.o");
    let c_path = temp_path("main.c");
    let exe_path = temp_path("exe");
    std::fs::write(&obj_path, &obj.bytes).unwrap();
    std::fs::write(
        &c_path,
        b"extern int k2_add(int, int);\n\
          int main(void){ return k2_add(40, 2) == 42 ? 0 : 1; }\n",
    )
    .unwrap();

    let link = Command::new(&cc)
        .arg("-no-pie")
        .arg("-o")
        .arg(&exe_path)
        .arg(&obj_path)
        .arg(&c_path)
        .output()
        .expect("link");
    assert!(
        link.status.success(),
        "link failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let run = Command::new(&exe_path).output().expect("run");
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&exe_path);
    assert_eq!(run.status.code(), Some(0), "C harness returns 0 (40+2==42)");
}

// =========================================================================
//  T3 — @sizeOf / @alignOf match the C ABI (compile + run a C comparison)
// =========================================================================

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn sizeof_matches_c_abi() {
    let Some(cc) = find_cc() else {
        eprintln!("skipping: no C compiler found");
        return;
    };
    // k2: print @sizeOf/@alignOf of three representative extern structs.
    let prog = lower(
        r#"
        const Point = extern struct { x: c_int, y: c_int };
        const Mixed = extern struct { a: c_char, b: c_long, c: c_int };
        const Ptrish = extern struct { p: *const u8, n: usize };
        const std = @import("std");
        pub fn main(sys: *System) !void {
            const out = sys.io.stdout();
            try out.print("{d} {d}\n", .{ @sizeOf(Point), @alignOf(Point) });
            try out.print("{d} {d}\n", .{ @sizeOf(Mixed), @alignOf(Mixed) });
            try out.print("{d} {d}\n", .{ @sizeOf(Ptrish), @alignOf(Ptrish) });
        }
        "#,
    );
    // Run the k2 program natively (freestanding) to capture its output.
    let k2_out = run_native_freestanding(&prog);

    // The equivalent C program.
    let c_src = r#"
        #include <stdio.h>
        #include <stddef.h>
        struct Point { int x, y; };
        struct Mixed { char a; long b; int c; };
        struct Ptrish { const unsigned char *p; size_t n; };
        int main(void) {
            printf("%zu %zu\n", sizeof(struct Point), _Alignof(struct Point));
            printf("%zu %zu\n", sizeof(struct Mixed), _Alignof(struct Mixed));
            printf("%zu %zu\n", sizeof(struct Ptrish), _Alignof(struct Ptrish));
            return 0;
        }
    "#;
    let c_path = temp_path("sz.c");
    let c_exe = temp_path("sz");
    std::fs::write(&c_path, c_src).unwrap();
    let comp = Command::new(&cc)
        .arg("-o")
        .arg(&c_exe)
        .arg(&c_path)
        .output()
        .expect("compile C");
    assert!(comp.status.success(), "C compile failed");
    let c_out = Command::new(&c_exe).output().expect("run C").stdout;
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&c_exe);

    assert_eq!(
        String::from_utf8_lossy(&k2_out),
        String::from_utf8_lossy(&c_out),
        "k2 @sizeOf/@alignOf must equal C sizeof/_Alignof"
    );
}

// =========================================================================
//  Helpers that shell out to cc / run binaries
// =========================================================================

/// Links a relocatable object (plus optional extra inputs) with `cc -no-pie`,
/// runs the result, and returns `(exit_code, stdout_bytes)`.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn link_and_run(cc: &str, obj_bytes: &[u8], extra: &[&str]) -> (i32, Vec<u8>) {
    let obj_path = temp_path("o");
    let exe_path = temp_path("exe");
    std::fs::write(&obj_path, obj_bytes).unwrap();
    let link = Command::new(cc)
        .arg("-no-pie")
        .arg("-o")
        .arg(&exe_path)
        .arg(&obj_path)
        .args(extra)
        .output()
        .expect("link driver");
    assert!(
        link.status.success(),
        "link failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let run = Command::new(&exe_path).output().expect("run linked binary");
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);
    (run.status.code().unwrap_or(-1), run.stdout)
}

/// Compiles `prog` to a freestanding static ELF, writes it, runs it, and returns
/// stdout (used to capture the `@sizeOf` print output without libc).
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn run_native_freestanding(prog: &MirProgram) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    let img =
        k2_codegen::compile_program_to_elf_for(prog, k2_codegen::Target::X86_64Linux).unwrap();
    let path = temp_path("elf");
    std::fs::write(&path, &img.bytes).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    let out = Command::new(&path).output().expect("run freestanding ELF");
    let _ = std::fs::remove_file(&path);
    out.stdout
}

/// Silence unused-helper warnings on non-x86-64 hosts (the run helpers are
/// `#[cfg]`-gated; `Path` import otherwise looks unused there).
#[allow(dead_code)]
fn _unused(_p: &Path) {}
