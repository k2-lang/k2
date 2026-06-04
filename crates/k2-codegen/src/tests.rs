//! Tests for the pure-std x86-64 native backend.
//!
//! Three tiers:
//!
//! 1. **Encoder** unit tests — assemble one instruction and assert the exact
//!    bytes against a known-good encoding captured from the system assembler
//!    (`as`/`objdump`). Host-independent: they run on every platform.
//! 2. **ELF validity** tests — write a tiny image and parse the header / program
//!    headers back, asserting the magic, class, type, machine, entry, and segment
//!    invariants. Host-independent.
//! 3. **Native execution** tests — compile a real k2 program through the front
//!    end, run the emitted ELF, and assert its exit code / stdout, including a
//!    **differential** check that native output matches the VM. These are gated
//!    `#[cfg(all(target_arch = "x86_64", target_os = "linux"))]` so they run on
//!    CI (x86-64 Linux) and are simply absent on other hosts.

use crate::encode::{Asm, Cc, LabelId};
use crate::reg::Gpr;

// =========================================================================
//  Tier 1 — encoder: instruction -> exact bytes
// =========================================================================

/// Builds an `Asm`, runs `f`, and returns the emitted code bytes.
fn enc(f: impl FnOnce(&mut Asm)) -> Vec<u8> {
    let mut a = Asm::new();
    f(&mut a);
    a.code().to_vec()
}

#[test]
fn mov_rr_bytes() {
    // mov rax, rsp -> 48 89 e0  (src=rsp in reg field, dst=rax in rm).
    assert_eq!(enc(|a| a.mov_rr(Gpr::Rax, Gpr::Rsp)), [0x48, 0x89, 0xe0]);
    // mov rdi, rax -> 48 89 c7.
    assert_eq!(enc(|a| a.mov_rr(Gpr::Rdi, Gpr::Rax)), [0x48, 0x89, 0xc7]);
    // mov rbp, rsp -> 48 89 e5.
    assert_eq!(enc(|a| a.mov_rr(Gpr::Rbp, Gpr::Rsp)), [0x48, 0x89, 0xe5]);
}

#[test]
fn mov_immediate_path_selection() {
    // Small (fits i32): 48 c7 c0 01 00 00 00.
    assert_eq!(
        enc(|a| a.mov_ri(Gpr::Rax, 1)),
        [0x48, 0xc7, 0xc0, 0x01, 0x00, 0x00, 0x00]
    );
    // SYS_exit = 60: 48 c7 c0 3c 00 00 00.
    assert_eq!(
        enc(|a| a.mov_ri(Gpr::Rax, 60)),
        [0x48, 0xc7, 0xc0, 0x3c, 0x00, 0x00, 0x00]
    );
    // Large (needs imm64): 48 b8 89 67 45 23 01 00 00 00.
    assert_eq!(
        enc(|a| a.mov_ri(Gpr::Rax, 0x1_2345_6789)),
        [0x48, 0xb8, 0x89, 0x67, 0x45, 0x23, 0x01, 0x00, 0x00, 0x00]
    );
}

#[test]
fn mov_load_store_disp8_and_disp32() {
    // mov rax, [rbp-8]  -> 48 8b 45 f8.
    assert_eq!(enc(|a| a.mov_load(Gpr::Rax, -8)), [0x48, 0x8b, 0x45, 0xf8]);
    // mov [rbp-8], rax  -> 48 89 45 f8.
    assert_eq!(enc(|a| a.mov_store(-8, Gpr::Rax)), [0x48, 0x89, 0x45, 0xf8]);
    // mov rax, [rbp-200] -> disp32: 48 8b 85 38 ff ff ff.
    assert_eq!(
        enc(|a| a.mov_load(Gpr::Rax, -200)),
        [0x48, 0x8b, 0x85, 0x38, 0xff, 0xff, 0xff]
    );
    // mov r8, [rbp-8] -> REX.R: 4c 8b 45 f8.
    assert_eq!(enc(|a| a.mov_load(Gpr::R8, -8)), [0x4c, 0x8b, 0x45, 0xf8]);
}

#[test]
fn alu_rr_bytes() {
    assert_eq!(enc(|a| a.add_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x01, 0xc8]);
    assert_eq!(enc(|a| a.sub_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x29, 0xc8]);
    assert_eq!(enc(|a| a.and_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x21, 0xc8]);
    assert_eq!(enc(|a| a.or_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x09, 0xc8]);
    assert_eq!(enc(|a| a.xor_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x31, 0xc8]);
    assert_eq!(enc(|a| a.cmp_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x39, 0xc8]);
    assert_eq!(enc(|a| a.test_rr(Gpr::Rax, Gpr::Rax)), [0x48, 0x85, 0xc0]);
    // xor rdi, rdi -> 48 31 ff.
    assert_eq!(enc(|a| a.xor_rr(Gpr::Rdi, Gpr::Rdi)), [0x48, 0x31, 0xff]);
}

#[test]
fn mul_div_neg_not_bytes() {
    // imul rax, rcx -> 48 0f af c1.
    assert_eq!(
        enc(|a| a.imul_rr(Gpr::Rax, Gpr::Rcx)),
        [0x48, 0x0f, 0xaf, 0xc1]
    );
    // cqo -> 48 99.
    assert_eq!(enc(|a| a.cqo()), [0x48, 0x99]);
    // idiv rcx -> 48 f7 f9.
    assert_eq!(enc(|a| a.idiv_r(Gpr::Rcx)), [0x48, 0xf7, 0xf9]);
    // neg rax -> 48 f7 d8.
    assert_eq!(enc(|a| a.neg_r(Gpr::Rax)), [0x48, 0xf7, 0xd8]);
    // not rax -> 48 f7 d0.
    assert_eq!(enc(|a| a.not_r(Gpr::Rax)), [0x48, 0xf7, 0xd0]);
}

#[test]
fn unsigned_div_mul_bytes() {
    // div rcx (REX.W F7 /6) -> 48 f7 f1 — the unsigned 64-bit division path.
    assert_eq!(enc(|a| a.div_r(Gpr::Rcx)), [0x48, 0xf7, 0xf1]);
    // mul rcx (REX.W F7 /4) -> 48 f7 e1 — unsigned 64-bit overflow multiply.
    assert_eq!(enc(|a| a.mul_r(Gpr::Rcx)), [0x48, 0xf7, 0xe1]);
    // imul rcx (REX.W F7 /5) -> 48 f7 e9 — signed 64-bit overflow multiply.
    assert_eq!(enc(|a| a.imul_r1(Gpr::Rcx)), [0x48, 0xf7, 0xe9]);
    // xor rdx, rdx (the unsigned-division high-half zero) -> 48 31 d2.
    assert_eq!(enc(|a| a.zero_rdx()), [0x48, 0x31, 0xd2]);
}

#[test]
fn mov_rr32_zero_extends_bytes() {
    // mov eax, eax (32-bit, NO REX.W) -> 89 c0. This zero-extends bits 32-63,
    // the correct u32 width normalization (a REX.W AND-mask would sign-extend
    // 0xFFFFFFFF to all-ones and mask nothing).
    assert_eq!(enc(|a| a.mov_rr32(Gpr::Rax, Gpr::Rax)), [0x89, 0xc0]);
    // mov ecx, ecx -> 89 c9.
    assert_eq!(enc(|a| a.mov_rr32(Gpr::Rcx, Gpr::Rcx)), [0x89, 0xc9]);
    // mov r8d, r8d -> 45 89 c0 (REX.B + REX.R, but NOT REX.W).
    assert_eq!(enc(|a| a.mov_rr32(Gpr::R8, Gpr::R8)), [0x45, 0x89, 0xc0]);
}

#[test]
fn shift_bytes() {
    // shl rax, cl -> 48 d3 e0.
    assert_eq!(enc(|a| a.shl_cl(Gpr::Rax)), [0x48, 0xd3, 0xe0]);
    // shr rax, cl -> 48 d3 e8.
    assert_eq!(enc(|a| a.shr_cl(Gpr::Rax)), [0x48, 0xd3, 0xe8]);
    // sar rax, cl -> 48 d3 f8.
    assert_eq!(enc(|a| a.sar_cl(Gpr::Rax)), [0x48, 0xd3, 0xf8]);
}

#[test]
fn immediate_alu_bytes() {
    // and rax, 0xff -> 48 81 e0 ff 00 00 00 (long /4 form, not the AL short form).
    assert_eq!(
        enc(|a| a.and_ri(Gpr::Rax, 0xff)),
        [0x48, 0x81, 0xe0, 0xff, 0x00, 0x00, 0x00]
    );
    // xor rax, 1 -> 48 81 f0 01 00 00 00.
    assert_eq!(
        enc(|a| a.xor_ri(Gpr::Rax, 1)),
        [0x48, 0x81, 0xf0, 0x01, 0x00, 0x00, 0x00]
    );
    // cmp rax, 5 -> 48 81 f8 05 00 00 00.
    assert_eq!(
        enc(|a| a.cmp_ri(Gpr::Rax, 5)),
        [0x48, 0x81, 0xf8, 0x05, 0x00, 0x00, 0x00]
    );
    // sub rsp, 32 -> 48 81 ec 20 00 00 00.
    assert_eq!(
        enc(|a| a.sub_rsp_imm(32)),
        [0x48, 0x81, 0xec, 0x20, 0x00, 0x00, 0x00]
    );
    // add rsp, 32 -> 48 81 c4 20 00 00 00.
    assert_eq!(
        enc(|a| a.add_rsp_imm(32)),
        [0x48, 0x81, 0xc4, 0x20, 0x00, 0x00, 0x00]
    );
}

#[test]
fn setcc_and_movzx_bytes() {
    // sete al -> 0f 94 c0.
    assert_eq!(enc(|a| a.setcc_al(Cc::E)), [0x0f, 0x94, 0xc0]);
    // setne al -> 0f 95 c0.
    assert_eq!(enc(|a| a.setcc_al(Cc::Ne)), [0x0f, 0x95, 0xc0]);
    // setl al -> 0f 9c c0.
    assert_eq!(enc(|a| a.setcc_al(Cc::L)), [0x0f, 0x9c, 0xc0]);
    // setb al -> 0f 92 c0.
    assert_eq!(enc(|a| a.setcc_al(Cc::B)), [0x0f, 0x92, 0xc0]);
    // seta al -> 0f 97 c0.
    assert_eq!(enc(|a| a.setcc_al(Cc::A)), [0x0f, 0x97, 0xc0]);
    // seto al -> 0f 90 c0 (signed overflow flag, for the 64-bit overflow check).
    assert_eq!(enc(|a| a.setcc_al(Cc::O)), [0x0f, 0x90, 0xc0]);
    // setc al -> 0f 92 c0 (carry flag, for the unsigned 64-bit overflow check).
    assert_eq!(enc(|a| a.setcc_al(Cc::C)), [0x0f, 0x92, 0xc0]);
    // movzx rax, al -> 48 0f b6 c0.
    assert_eq!(enc(|a| a.movzx_al(Gpr::Rax)), [0x48, 0x0f, 0xb6, 0xc0]);
}

#[test]
fn all_setcc_variants_distinct() {
    // The tttn nibbles must match the verified table; assert each opcode byte.
    let cases = [
        (Cc::E, 0x94u8),
        (Cc::Ne, 0x95),
        (Cc::L, 0x9c),
        (Cc::Le, 0x9e),
        (Cc::G, 0x9f),
        (Cc::Ge, 0x9d),
        (Cc::B, 0x92),
        (Cc::Be, 0x96),
        (Cc::A, 0x97),
        (Cc::Ae, 0x93),
    ];
    for (cc, op) in cases {
        assert_eq!(enc(|a| a.setcc_al(cc)), [0x0f, op, 0xc0], "setcc {cc:?}");
    }
}

#[test]
fn width_fixup_bytes() {
    // movsxd rax, ecx -> 48 63 c1.
    assert_eq!(enc(|a| a.movsxd(Gpr::Rax, Gpr::Rcx)), [0x48, 0x63, 0xc1]);
    // movzx rax, cl -> 48 0f b6 c1.
    assert_eq!(
        enc(|a| a.movzx8(Gpr::Rax, Gpr::Rcx)),
        [0x48, 0x0f, 0xb6, 0xc1]
    );
    // movsx rax, cl -> 48 0f be c1.
    assert_eq!(
        enc(|a| a.movsx8(Gpr::Rax, Gpr::Rcx)),
        [0x48, 0x0f, 0xbe, 0xc1]
    );
    // movzx rax, cx -> 48 0f b7 c1.
    assert_eq!(
        enc(|a| a.movzx16(Gpr::Rax, Gpr::Rcx)),
        [0x48, 0x0f, 0xb7, 0xc1]
    );
    // movsx rax, cx -> 48 0f bf c1.
    assert_eq!(
        enc(|a| a.movsx16(Gpr::Rax, Gpr::Rcx)),
        [0x48, 0x0f, 0xbf, 0xc1]
    );
}

#[test]
fn stack_and_control_bytes() {
    // lea rax, [rbp-16] -> 48 8d 45 f0.
    assert_eq!(enc(|a| a.lea_rbp(Gpr::Rax, -16)), [0x48, 0x8d, 0x45, 0xf0]);
    // push rbp -> 55 ; pop rbp -> 5d.
    assert_eq!(enc(|a| a.push(Gpr::Rbp)), [0x55]);
    assert_eq!(enc(|a| a.pop(Gpr::Rbp)), [0x5d]);
    // push r12 -> 41 54 (REX.B).
    assert_eq!(enc(|a| a.push(Gpr::R12)), [0x41, 0x54]);
    // ret / leave / syscall.
    assert_eq!(enc(|a| a.ret()), [0xc3]);
    assert_eq!(enc(|a| a.leave()), [0xc9]);
    assert_eq!(enc(|a| a.syscall()), [0x0f, 0x05]);
}

#[test]
fn forward_jump_patched_rel32() {
    // jmp .L ; (2-byte filler) ; .L:  -> the rel32 must equal target-(at+4).
    let mut a = Asm::new();
    a.reserve_labels(1);
    a.jmp(LabelId(0)); // E9 + 4-byte hole at offset 1..5
    a.ret(); // one byte filler at offset 5
    a.bind_label(LabelId(0)); // target at offset 6
    let (code, fixups) = a.finish();
    assert!(
        fixups.is_empty(),
        "local label must be resolved in finish()"
    );
    // E9 at 0; hole at 1; jmp ends at 5; target=6 -> rel32 = 6 - 5 = 1.
    assert_eq!(code[0], 0xe9);
    assert_eq!(&code[1..5], &1i32.to_le_bytes());
}

#[test]
fn backward_jump_patched_rel32() {
    // .L: ret ; jcc .L  -> negative rel32.
    let mut a = Asm::new();
    a.reserve_labels(1);
    a.bind_label(LabelId(0)); // target at offset 0
    a.ret(); // offset 0, one byte
    a.jcc(Cc::Ne, LabelId(0)); // 0F 85 at 1..3, hole at 3..7, ends at 7
    let (code, _) = a.finish();
    assert_eq!(&code[0..3], &[0xc3, 0x0f, 0x85]);
    // target=0; site end=7 -> rel32 = 0 - 7 = -7.
    assert_eq!(&code[3..7], &(-7i32).to_le_bytes());
}

#[test]
fn call_fixup_survives_finish() {
    // A cross-function call leaves an unresolved Call fixup for the layout pass.
    let mut a = Asm::new();
    a.reserve_labels(0);
    a.call_fn(k2_mir::FnId(3));
    let (code, fixups) = a.finish();
    assert_eq!(code[0], 0xe8);
    assert_eq!(fixups.len(), 1);
    assert_eq!(fixups[0].at, 1);
    assert!(matches!(
        fixups[0].kind,
        crate::encode::FixupKind::Call(k2_mir::FnId(3))
    ));
}

#[test]
fn prologue_sequence_concatenation() {
    // A full prologue: push rbp; mov rbp, rsp; sub rsp, 0x20.
    let mut a = Asm::new();
    a.push(Gpr::Rbp);
    a.mov_rr(Gpr::Rbp, Gpr::Rsp);
    a.sub_rsp_imm(0x20);
    assert_eq!(
        a.code(),
        &[
            0x55, // push rbp
            0x48, 0x89, 0xe5, // mov rbp, rsp
            0x48, 0x81, 0xec, 0x20, 0x00, 0x00, 0x00 // sub rsp, 0x20
        ]
    );
}

// =========================================================================
//  Tier 2 — ELF validity
// =========================================================================

#[test]
fn elf_header_invariants_text_only() {
    // A single `nop` with no rodata -> one PT_LOAD.
    let img = crate::elf::write_elf(&[0x90], &[]);
    let b = &img.bytes;
    // Magic + class/data/version/osabi.
    assert_eq!(&b[0..4], &[0x7f, b'E', b'L', b'F']);
    assert_eq!(b[4], 2); // ELFCLASS64
    assert_eq!(b[5], 1); // ELFDATA2LSB
    assert_eq!(b[6], 1); // EI_VERSION
                         // e_type == ET_EXEC (2), e_machine == EM_X86_64 (0x3E).
    assert_eq!(u16::from_le_bytes([b[16], b[17]]), 2);
    assert_eq!(u16::from_le_bytes([b[18], b[19]]), 0x3e);
    // e_entry == 0x401000.
    assert_eq!(read_u64(b, 24), 0x40_1000);
    assert_eq!(img.text_vaddr, 0x40_1000);
    // e_phoff == 64, e_ehsize == 64, e_phentsize == 56.
    assert_eq!(read_u64(b, 32), 64);
    assert_eq!(u16::from_le_bytes([b[52], b[53]]), 64); // e_ehsize
    assert_eq!(u16::from_le_bytes([b[54], b[55]]), 56); // e_phentsize
    assert_eq!(u16::from_le_bytes([b[56], b[57]]), 1); // e_phnum (text only)
}

#[test]
fn elf_program_headers_two_segments() {
    // Code + a rodata string -> two PT_LOADs.
    let img = crate::elf::write_elf(&[0x90, 0x90], b"hi\n");
    let b = &img.bytes;
    assert_eq!(u16::from_le_bytes([b[56], b[57]]), 2); // e_phnum == 2

    // Phdr 0 (text, RX) starts at e_phoff = 64.
    let p0 = 64;
    assert_eq!(read_u32(b, p0), 1); // p_type == PT_LOAD
    assert_eq!(read_u32(b, p0 + 4), 5); // p_flags == PF_R|PF_X
    assert_eq!(read_u64(b, p0 + 8), 0); // p_offset
    assert_eq!(read_u64(b, p0 + 16), 0x40_0000); // p_vaddr (load base)
    assert_eq!(read_u64(b, p0 + 48), 0x1000); // p_align

    // Phdr 1 (rodata, R).
    let p1 = 64 + 56;
    assert_eq!(read_u32(b, p1), 1); // p_type
    assert_eq!(read_u32(b, p1 + 4), 4); // p_flags == PF_R
    let r_off = read_u64(b, p1 + 8);
    let r_vaddr = read_u64(b, p1 + 16);
    assert_eq!(read_u64(b, p1 + 48), 0x1000); // p_align
                                              // The kernel's mapping congruence: p_vaddr ≡ p_offset (mod p_align).
    assert_eq!(r_vaddr % 0x1000, r_off % 0x1000);
    assert_eq!(r_vaddr, img.rodata_vaddr);
    // p_filesz of the rodata segment equals the rodata length.
    assert_eq!(read_u64(b, p1 + 32), 3);

    // The rodata bytes actually live at their file offset.
    let off = r_off as usize;
    assert_eq!(&b[off..off + 3], b"hi\n");
}

#[test]
fn rodata_vaddr_is_next_page_after_text() {
    // .text starts at file offset 0x1000; rodata is rounded up to the next page
    // boundary after the text body.
    assert_eq!(crate::elf::rodata_vaddr_for(0), 0x40_1000);
    assert_eq!(crate::elf::rodata_vaddr_for(1), 0x40_2000);
    assert_eq!(crate::elf::rodata_vaddr_for(0x1000), 0x40_2000);
    assert_eq!(crate::elf::rodata_vaddr_for(0x1001), 0x40_3000);
}

/// Reads a little-endian `u64` from `b` at byte offset `at`.
fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
/// Reads a little-endian `u32` from `b` at byte offset `at`.
fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

// =========================================================================
//  Tier 3 — native execution (gated to x86-64 Linux)
// =========================================================================

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod exec {
    use k2_mir::{lower_program, BuildMode, MirProgram};

    /// Lowers a self-contained k2 source string to a verified `MirProgram`
    /// through the real front-end pipeline (parse -> resolve -> check -> lower ->
    /// optimize -> verify), mirroring `k2c run`. Panics with the front-end
    /// diagnostics on any error, so a test program that does not compile fails
    /// loudly rather than silently.
    fn lower(source: &str) -> MirProgram {
        lower_mode(source, BuildMode::Debug)
    }

    /// Like [`lower`], but with an explicit [`BuildMode`] — `ReleaseFast` elides
    /// the safety checks, exercising the wrapping (no-trap) arithmetic paths.
    fn lower_mode(source: &str, mode: BuildMode) -> MirProgram {
        // Inject the bundled std prelude exactly like the driver, so
        // `@import("std")` and the `*System` capability methods resolve.
        let mut combined = String::new();
        combined.push_str(source);
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

        let mut prog = lower_program(&pres.file, &resolved, typed, mode).expect("lowering failed");
        assert!(prog.is_ok(), "lowering diagnostics in test program");
        k2_opt::optimize(&mut prog, k2_opt::OptLevel::None);
        let problems = prog.verify();
        assert!(problems.is_empty(), "malformed MIR: {problems:?}");
        prog
    }

    /// Re-points `const X = @import("std")` to the synthetic std root (the same
    /// rewrite the CLI driver performs).
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

    /// Compiles `prog` to an ELF, writes it to a unique temp file, `chmod +x`-es
    /// it, runs it, and returns `(exit_code, stdout_bytes, stderr_bytes)`. The
    /// temp path is keyed by pid + a per-call atomic counter (not the wall clock),
    /// so two near-simultaneous calls — across parallel tests or within one test —
    /// never collide on the same inode (which would otherwise `ETXTBSY` when one
    /// run execs a file another is still writing).
    fn run_native(prog: &MirProgram) -> (i32, Vec<u8>, Vec<u8>) {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);

        let img = crate::compile_program_to_elf(prog).expect("codegen failed");
        let path =
            std::env::temp_dir().join(format!("k2_native_test_{}_{}", std::process::id(), n));
        std::fs::write(&path, &img.bytes).expect("write elf");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let out = Command::new(&path).output().expect("exec native binary");
        let _ = std::fs::remove_file(&path);
        let code = out.status.code().unwrap_or(-1);
        (code, out.stdout, out.stderr)
    }

    /// Runs `prog` on the VM, returning `(exit_code, stdout, stderr)`.
    fn run_vm(prog: &MirProgram) -> (i32, Vec<u8>, Vec<u8>) {
        let (_outcome, code, out, err) = k2_vm::run_captured(prog, k2_vm::RunArgs::new(prog.mode));
        (code, out, err)
    }

    /// Asserts the native run reproduces the VM's stdout exactly, returning both
    /// exit codes. (The two backends now share the integer-`main` exit-code
    /// convention, so the codes also agree — see [`assert_native_eq_vm`] for the
    /// stricter check.)
    fn assert_stdout_matches(prog: &MirProgram) -> (i32, i32) {
        let (n_code, n_out, _n_err) = run_native(prog);
        let (v_code, v_out, _v_err) = run_vm(prog);
        assert_eq!(
            String::from_utf8_lossy(&n_out),
            String::from_utf8_lossy(&v_out),
            "native stdout must match the VM"
        );
        (n_code, v_code)
    }

    /// The full differential invariant: the native backend and the VM must agree
    /// on **both** the process exit code and stdout for an in-subset program.
    /// Returns the shared exit code. This is the milestone's `native == VM`
    /// guarantee, asserted directly by the regression tests below.
    fn assert_native_eq_vm(prog: &MirProgram) -> i32 {
        let (n_code, n_out, _n_err) = run_native(prog);
        let (v_code, v_out, _v_err) = run_vm(prog);
        assert_eq!(
            String::from_utf8_lossy(&n_out),
            String::from_utf8_lossy(&v_out),
            "native stdout must match the VM"
        );
        assert_eq!(n_code, v_code, "native exit code must match the VM");
        n_code
    }

    // ---- (a) integer compute -> exit code ----

    #[test]
    fn integer_compute_exit_code() {
        // A loop/branch/arith kernel whose result becomes the process exit code.
        // The native `_start` propagates main's integer result directly.
        let prog = lower(
            "fn compute() i32 { var s: i32 = 0; var i: i32 = 0; \
             while (i < 10) : (i += 1) { s += i; } return s; } \
             pub fn main() u8 { return @intCast(compute()); }",
        );
        let (code, _out, _err) = run_native(&prog);
        assert_eq!(code, 45, "0+1+..+9 == 45 as the exit code");
    }

    #[test]
    fn recursion_exit_code() {
        // (b) recursion computing a value returned as the exit code.
        let prog = lower(
            "fn fib(n: u8) u8 { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); } \
             pub fn main() u8 { return fib(10); }",
        );
        let (code, _out, _err) = run_native(&prog);
        assert_eq!(code, 55, "fib(10) == 55");
    }

    #[test]
    fn conditionals_and_switch_exit_code() {
        // Conditionals + a switch computing a value.
        let prog = lower(
            "fn pick(n: u8) u8 { \
                switch (n) { 0 => { return 10; }, 1 => { return 20; }, else => { return 30; } } \
             } \
             pub fn main() u8 { return pick(1) + pick(5); }",
        );
        let (code, _out, _err) = run_native(&prog);
        assert_eq!(code, 50, "pick(1)=20 + pick(5)=30 == 50");
    }

    #[test]
    fn arithmetic_bitwise_shift_exit_code() {
        let prog = lower(
            "pub fn main() u8 { \
                var x: u8 = 6; var y: u8 = 3; \
                var a: u8 = x * y;        \
                a = a - 1;                \
                a = a & 0x1f;             \
                a = a | 0x40;             \
                a = a ^ 0x01;             \
                a = a >> 1;               \
                return a; }",
        );
        // 6*3=18 ; -1=17 ; &0x1f=17 ; |0x40=81 ; ^1=80 ; >>1=40.
        let (code, _out, _err) = run_native(&prog);
        assert_eq!(code, 40);
    }

    // ---- (c) fixed byte string to stdout via write ----

    #[test]
    fn hello_string_to_stdout() {
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { \
                const out = sys.io.stdout(); \
                try out.print(\"Hello, k2!\\n\", .{}); }",
        );
        let (n_code, v_code) = assert_stdout_matches(&prog);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"Hello, k2!\n");
        assert_eq!(n_code, 0);
        assert_eq!(v_code, 0);
    }

    #[test]
    fn multiline_string_to_stdout() {
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { \
                const out = sys.io.stdout(); \
                try out.print(\"line one\\nline two\\n\", .{}); }",
        );
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"line one\nline two\n");
        assert_stdout_matches(&prog);
    }

    #[test]
    fn string_to_stderr() {
        // stderr goes to fd 2; assert it lands on stderr, not stdout.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { \
                const e = sys.io.stderr(); \
                try e.print(\"diag\\n\", .{}); }",
        );
        let (code, out, err) = run_native(&prog);
        assert_eq!(code, 0);
        assert!(out.is_empty(), "nothing on stdout");
        assert_eq!(err, b"diag\n");
    }

    // ---- Trap / panic exit-code parity ----

    #[test]
    fn overflow_trap_parity() {
        // u8 overflow in Debug mode -> trap -> exit 134 in both VM and native.
        let prog = lower("pub fn main() u8 { var x: u8 = 255; x = x + 1; return x; }");
        let (n_code, _n_out, _n_err) = run_native(&prog);
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(n_code, 134, "native overflow trap exits 134");
        assert_eq!(v_code, 134, "VM overflow trap exits 134");
    }

    // ---- Differential sweep over a small subset corpus ----

    #[test]
    fn differential_stdout_corpus() {
        // Several fixed-string programs: native stdout must equal the VM's, and
        // both must exit 0.
        let programs = [
            "const std = @import(\"std\");\npub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"a\\n\", .{}); }",
            "const std = @import(\"std\");\npub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"hello world\\n\", .{}); }",
            "const std = @import(\"std\");\npub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"x\\ny\\nz\\n\", .{}); }",
            "const std = @import(\"std\");\npub fn main(sys: *System) !void { const o = sys.io.stdout(); try o.print(\"k2 native backend works\\n\", .{}); }",
        ];
        for src in programs {
            let prog = lower(src);
            let (n_code, v_code) = assert_stdout_matches(&prog);
            assert_eq!(n_code, 0, "native exit 0 for `{src}`");
            assert_eq!(v_code, 0, "vm exit 0 for `{src}`");
        }
    }

    #[test]
    fn unsupported_program_is_clean_error() {
        // A float program is outside the subset: codegen must return a clean
        // Unsupported error, never panic the host process.
        let prog = lower("pub fn main() u8 { var f: f64 = 1.5; return @intFromFloat(f); }");
        match crate::compile_program_to_elf(&prog) {
            Err(crate::CodegenError::Unsupported(_)) => {}
            Err(crate::CodegenError::NoMain) => panic!("expected Unsupported, got NoMain"),
            Ok(_) => panic!("expected Unsupported, but codegen succeeded"),
        }
    }

    // =====================================================================
    //  v0.14 differential regression tests (native == VM on correct
    //  semantics). Each pins a previously-divergent behaviour from the
    //  review findings.
    // =====================================================================

    // ---- Finding 1: u32 unsigned width normalization ----

    #[test]
    fn diff_u32_arithmetic_wraps_to_32_bits() {
        // A u32 product that exceeds 2^32 must wrap to 32 bits identically on both
        // backends. ReleaseFast elides the overflow trap so the wrap is observed;
        // 65536 * 65536 == 2^32 wraps to 0. (Previously native's REX.W AND-mask
        // sign-extended 0xFFFFFFFF to all-ones and masked nothing, so the value
        // never wrapped.)
        let prog = lower_mode(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var a: u32 = 65536; var b: u32 = 65536; const p = a * b; \
                if (p == 0) { try o.print(\"WRAP0\\n\", .{}); } \
                else { try o.print(\"WRAPBAD\\n\", .{}); } }",
            BuildMode::ReleaseFast,
        );
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"WRAP0\n");
    }

    #[test]
    fn diff_u32_truncate_from_u64() {
        // @truncate(u64 = 2^32) to u32 is 0 on both backends.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var big: u64 = 4294967296; const t: u32 = @truncate(big); \
                if (t == 0) { try o.print(\"TRUNC0\\n\", .{}); } \
                else { try o.print(\"TRUNCBAD\\n\", .{}); } }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    // ---- Finding 2: unsigned u64/usize division & remainder ----

    #[test]
    fn diff_u64_unsigned_division() {
        // u64::MAX / 2 == 2^63 - 1 (high bit set). Native previously used signed
        // idiv and computed -1/2 == 0.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var a: u64 = 18446744073709551615; var b: u64 = 2; const q = a / b; \
                if (q == 9223372036854775807) { try o.print(\"OK\\n\", .{}); } \
                else { try o.print(\"WRONG\\n\", .{}); } }",
        );
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"OK\n");
    }

    #[test]
    fn diff_u64_unsigned_remainder() {
        // u64::MAX % 10 == 5 on both backends (native's signed idiv gave garbage).
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var a: u64 = 18446744073709551615; var b: u64 = 10; const r = a % b; \
                if (r == 5) { try o.print(\"REM5\\n\", .{}); } \
                else { try o.print(\"REMBAD\\n\", .{}); } }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_u64_division_min_pattern_no_sigfpe() {
        // 2^63 / u64::MAX == 0. As signed idiv this is the i64::MIN / -1 bit
        // pattern, which raises #DE (SIGFPE); unsigned div computes 0 cleanly.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var a: u64 = 9223372036854775808; var b: u64 = 18446744073709551615; \
                const q = a / b; \
                if (q == 0) { try o.print(\"DIVOK\\n\", .{}); } \
                else { try o.print(\"DIVBAD\\n\", .{}); } }",
        );
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0, "no SIGFPE; clean exit 0");
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"DIVOK\n");
    }

    #[test]
    fn diff_signed_division_still_correct() {
        // The signed path is unchanged: -7 / 2 == -3, -7 % 2 == -1.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var a: i64 = -7; var b: i64 = 2; const q = a / b; const r = a % b; \
                if (q == -3 and r == -1) { try o.print(\"SOK\\n\", .{}); } \
                else { try o.print(\"SBAD\\n\", .{}); } }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    // ---- Finding 3: 64-bit arithmetic overflow trap parity ----

    #[test]
    fn diff_u64_add_overflow_traps() {
        // u64::MAX + 1 overflows: both backends trap (exit 134) in Debug.
        let prog = lower(
            "fn bad() u64 { var x: u64 = 18446744073709551615; var y: u64 = 1; return x + y; } \
             pub fn main() u64 { return bad(); }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_i64_add_overflow_traps() {
        // i64::MAX + 1 overflows: both trap.
        let prog = lower(
            "fn bad() i64 { var x: i64 = 9223372036854775807; var y: i64 = 1; return x + y; } \
             pub fn main() u64 { var r: i64 = bad(); return 0; }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_u64_mul_overflow_traps() {
        // u64::MAX * 2 overflows: both trap (unsigned `mul`, CF set).
        let prog = lower(
            "fn bad() u64 { var x: u64 = 18446744073709551615; var y: u64 = 2; return x * y; } \
             pub fn main() u64 { return bad(); }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_i64_mul_overflow_traps() {
        // i64::MAX * 2 overflows: both trap (signed one-operand `imul`, OF set).
        let prog = lower(
            "fn bad() i64 { var x: i64 = 9223372036854775807; var y: i64 = 2; return x * y; } \
             pub fn main() u64 { var r: i64 = bad(); return 0; }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_u64_nonoverflowing_op_does_not_trap() {
        // A genuine non-overflowing 64-bit op must NOT trap on either backend.
        let prog = lower(
            "const std = @import(\"std\");\n\
             fn ok() u64 { var x: u64 = 1000; var y: u64 = 2000; return x + y; } \
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); var r: u64 = ok(); \
                if (r == 3000) { try o.print(\"OK3000\\n\", .{}); } \
                else { try o.print(\"BAD\\n\", .{}); } }",
        );
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0, "non-overflowing 64-bit op must not trap");
    }

    #[test]
    fn diff_u64_to_i64_narrow_traps() {
        // @intCast(u64 > i64::MAX) -> i64 is lossy: both trap (64-bit narrow_fits).
        let prog = lower(
            "fn bad() i64 { var x: u64 = 18446744073709551615; return @intCast(x); } \
             pub fn main() u64 { var r: i64 = bad(); return 0; }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_i64_to_u64_narrow_traps() {
        // @intCast(i64 -1) -> u64 is lossy (negative): both trap.
        let prog = lower(
            "fn bad() u64 { var x: i64 = -1; return @intCast(x); } \
             pub fn main() u64 { return bad(); }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_u64_to_i64_narrow_in_range_ok() {
        // An in-range 64-bit signedness narrow does NOT trap.
        let prog = lower(
            "const std = @import(\"std\");\n\
             fn ok() i64 { var x: u64 = 100; return @intCast(x); } \
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); var r: i64 = ok(); \
                if (r == 100) { try o.print(\"OK100\\n\", .{}); } \
                else { try o.print(\"BAD\\n\", .{}); } }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    // ---- Finding 4: integer @bitCast / PtrReinterpret ----

    #[test]
    fn diff_bitcast_i8_to_u8() {
        // @bitCast(i8 -1) -> u8 is 255 on both backends (the VM previously kept -1).
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var x: i8 = -1; const r: u8 = @bitCast(x); \
                if (r == 255) { try o.print(\"255\\n\", .{}); } \
                else { try o.print(\"not255\\n\", .{}); } }",
        );
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"255\n");
    }

    #[test]
    fn diff_bitcast_u8_to_i8() {
        // @bitCast(u8 255) -> i8 is -1 on both backends.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                var x: u8 = 255; const r: i8 = @bitCast(x); \
                if (r == -1) { try o.print(\"NEG1\\n\", .{}); } \
                else { try o.print(\"POS\\n\", .{}); } }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    // ---- Finding 5: integer-returning main exit-code convention ----

    #[test]
    fn diff_integer_main_exit_code() {
        // `pub fn main() u8 { return 42; }` exits 42 on BOTH backends (the VM used
        // to ignore the integer result and exit 0).
        let prog = lower("pub fn main(sys: *System) u8 { return 42; }");
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 42);
    }

    #[test]
    fn diff_integer_main_exit_code_truncates() {
        // A return value above 255 truncates to its low 8 bits on both backends.
        let prog = lower("pub fn main(sys: *System) u32 { return 300; }");
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 300 % 256, "300 & 0xff == 44");
    }

    #[test]
    fn diff_void_main_exit_code_still_zero() {
        // An `!void` main still exits 0 on the success path on both backends.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                try o.print(\"hi\\n\", .{}); }",
        );
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }
}
