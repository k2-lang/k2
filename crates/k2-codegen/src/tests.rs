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
use crate::reg::{Gpr, Xmm};

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
fn adc_sbb_bytes() {
    // The two-limb 128-bit add/sub carry-propagation pair.
    // adc rax, rcx -> 48 11 c8 ; adc rdx, r11 -> 4c 11 da.
    assert_eq!(enc(|a| a.adc_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x11, 0xc8]);
    assert_eq!(enc(|a| a.adc_rr(Gpr::Rdx, Gpr::R11)), [0x4c, 0x11, 0xda]);
    // sbb rax, rcx -> 48 19 c8 ; sbb r14, r9 -> 4d 19 ce.
    assert_eq!(enc(|a| a.sbb_rr(Gpr::Rax, Gpr::Rcx)), [0x48, 0x19, 0xc8]);
    assert_eq!(enc(|a| a.sbb_rr(Gpr::R14, Gpr::R9)), [0x4d, 0x19, 0xce]);
}

#[test]
fn movq_xmm_r64_bytes() {
    // movq xmm0, rax  -> 66 48 0f 6e c0 (GPR bit pattern into xmm, no memory).
    assert_eq!(
        enc(|a| a.movq_xmm_r64(Xmm::Xmm0, Gpr::Rax)),
        [0x66, 0x48, 0x0f, 0x6e, 0xc0]
    );
    // movq xmm1, r11 -> 66 49 0f 6e cb (REX.B for r11).
    assert_eq!(
        enc(|a| a.movq_xmm_r64(Xmm::Xmm1, Gpr::R11)),
        [0x66, 0x49, 0x0f, 0x6e, 0xcb]
    );
    // movq xmm8, rcx -> 66 4c 0f 6e c1 (REX.R for xmm8).
    assert_eq!(
        enc(|a| a.movq_xmm_r64(Xmm::Xmm8, Gpr::Rcx)),
        [0x66, 0x4c, 0x0f, 0x6e, 0xc1]
    );
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

// ---- v0.15 encoder additions: byte-exact vs the system assembler ----

#[test]
fn lea_mem_arbitrary_base_bytes() {
    // lea rax, [rcx+8]  -> 48 8d 41 08.
    assert_eq!(
        enc(|a| a.lea_mem(Gpr::Rax, Gpr::Rcx, 8)),
        [0x48, 0x8d, 0x41, 0x08]
    );
    // lea rsi, [rsp+0] (rsp base needs a SIB; our encoder always emits an explicit
    // disp8, so [rsp+0] is `mod=01 rm=100 SIB(base=rsp) disp8=0`) -> 48 8d 74 24 00.
    // (The system assembler picks the shorter mod=00 form `48 8d 34 24`; both
    // address [rsp] identically. We assert our own deterministic encoding.)
    assert_eq!(
        enc(|a| a.lea_mem(Gpr::Rsi, Gpr::Rsp, 0)),
        [0x48, 0x8d, 0x74, 0x24, 0x00]
    );
    // mov rax, [r13+0] (r13 base, disp 0 must still emit disp8) -> 49 8b 45 00.
    assert_eq!(
        enc(|a| a.mov_load_mem(Gpr::Rax, Gpr::R13, 0)),
        [0x49, 0x8b, 0x45, 0x00]
    );
    // mov rax, [r12+8] (r12 base needs a SIB) -> 49 8b 44 24 08.
    assert_eq!(
        enc(|a| a.mov_load_mem(Gpr::Rax, Gpr::R12, 8)),
        [0x49, 0x8b, 0x44, 0x24, 0x08]
    );
}

#[test]
fn sized_mem_loads_bytes() {
    // movzx rax, byte [rcx+4]  -> 48 0f b6 41 04.
    assert_eq!(
        enc(|a| a.movzx8_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x48, 0x0f, 0xb6, 0x41, 0x04]
    );
    // movsx rax, byte [rcx+4]  -> 48 0f be 41 04.
    assert_eq!(
        enc(|a| a.movsx8_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x48, 0x0f, 0xbe, 0x41, 0x04]
    );
    // movzx rax, word [rcx+4]  -> 48 0f b7 41 04.
    assert_eq!(
        enc(|a| a.movzx16_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x48, 0x0f, 0xb7, 0x41, 0x04]
    );
    // movsx rax, word [rcx+4]  -> 48 0f bf 41 04.
    assert_eq!(
        enc(|a| a.movsx16_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x48, 0x0f, 0xbf, 0x41, 0x04]
    );
    // mov eax, [rcx+4] (32-bit, zero-extends, no REX.W) -> 8b 41 04.
    assert_eq!(
        enc(|a| a.mov_load32_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x8b, 0x41, 0x04]
    );
    // movsxd rax, [rcx+4]  -> 48 63 41 04.
    assert_eq!(
        enc(|a| a.movsxd_mem(Gpr::Rax, Gpr::Rcx, 4)),
        [0x48, 0x63, 0x41, 0x04]
    );
}

#[test]
fn sized_mem_stores_bytes() {
    // mov byte [rcx+4], al  -> 88 41 04.
    assert_eq!(
        enc(|a| a.mov_store8_mem(Gpr::Rcx, 4, Gpr::Rax)),
        [0x88, 0x41, 0x04]
    );
    // mov byte [rcx+4], sil (needs REX for spl/sil) -> 40 88 71 04.
    assert_eq!(
        enc(|a| a.mov_store8_mem(Gpr::Rcx, 4, Gpr::Rsi)),
        [0x40, 0x88, 0x71, 0x04]
    );
    // mov word [rcx+4], ax  -> 66 89 41 04.
    assert_eq!(
        enc(|a| a.mov_store16_mem(Gpr::Rcx, 4, Gpr::Rax)),
        [0x66, 0x89, 0x41, 0x04]
    );
    // mov dword [rcx+4], eax  -> 89 41 04.
    assert_eq!(
        enc(|a| a.mov_store32_mem(Gpr::Rcx, 4, Gpr::Rax)),
        [0x89, 0x41, 0x04]
    );
    // mov [rcx+8], rax  -> 48 89 41 08.
    assert_eq!(
        enc(|a| a.mov_store_mem(Gpr::Rcx, 8, Gpr::Rax)),
        [0x48, 0x89, 0x41, 0x08]
    );
}

#[test]
fn rep_movsb_and_index_arith_bytes() {
    // rep movsb -> f3 a4.
    assert_eq!(enc(|a| a.rep_movsb()), [0xf3, 0xa4]);
    // shl rax, 3 -> 48 c1 e0 03.
    assert_eq!(enc(|a| a.shl_ri(Gpr::Rax, 3)), [0x48, 0xc1, 0xe0, 0x03]);
    // add rax, 16 (imm32 form) -> 48 81 c0 10 00 00 00.
    assert_eq!(
        enc(|a| a.add_ri(Gpr::Rax, 16)),
        [0x48, 0x81, 0xc0, 0x10, 0x00, 0x00, 0x00]
    );
    // imul rcx, rcx, 12 (imm32 form) -> 48 69 c9 0c 00 00 00.
    assert_eq!(
        enc(|a| a.imul_rri(Gpr::Rcx, Gpr::Rcx, 12)),
        [0x48, 0x69, 0xc9, 0x0c, 0x00, 0x00, 0x00]
    );
}

#[test]
fn sse_double_family_bytes() {
    // movsd xmm0, [rax] — our encoder emits an explicit disp8=0 (mod=01) rather
    // than the assembler's shorter mod=00; both address [rax]. -> f2 0f 10 40 00.
    assert_eq!(
        enc(|a| a.movsd_load(Xmm::Xmm0, Gpr::Rax, 0)),
        [0xf2, 0x0f, 0x10, 0x40, 0x00]
    );
    // movsd [rax], xmm0  -> f2 0f 11 40 00.
    assert_eq!(
        enc(|a| a.movsd_store(Gpr::Rax, 0, Xmm::Xmm0)),
        [0xf2, 0x0f, 0x11, 0x40, 0x00]
    );
    // movsd xmm8, [rax] (REX.R) -> f2 44 0f 10 40 00.
    assert_eq!(
        enc(|a| a.movsd_load(Xmm::Xmm8, Gpr::Rax, 0)),
        [0xf2, 0x44, 0x0f, 0x10, 0x40, 0x00]
    );
    // addsd/subsd/mulsd/divsd xmm0, xmm1.
    assert_eq!(
        enc(|a| a.addsd(Xmm::Xmm0, Xmm::Xmm1)),
        [0xf2, 0x0f, 0x58, 0xc1]
    );
    assert_eq!(
        enc(|a| a.subsd(Xmm::Xmm0, Xmm::Xmm1)),
        [0xf2, 0x0f, 0x5c, 0xc1]
    );
    assert_eq!(
        enc(|a| a.mulsd(Xmm::Xmm0, Xmm::Xmm1)),
        [0xf2, 0x0f, 0x59, 0xc1]
    );
    assert_eq!(
        enc(|a| a.divsd(Xmm::Xmm0, Xmm::Xmm1)),
        [0xf2, 0x0f, 0x5e, 0xc1]
    );
    // ucomisd xmm0, xmm1  -> 66 0f 2e c1.
    assert_eq!(
        enc(|a| a.ucomisd(Xmm::Xmm0, Xmm::Xmm1)),
        [0x66, 0x0f, 0x2e, 0xc1]
    );
    // cvtsi2sd xmm0, rax  -> f2 48 0f 2a c0.
    assert_eq!(
        enc(|a| a.cvtsi2sd(Xmm::Xmm0, Gpr::Rax)),
        [0xf2, 0x48, 0x0f, 0x2a, 0xc0]
    );
    // cvttsd2si rax, xmm0  -> f2 48 0f 2c c0.
    assert_eq!(
        enc(|a| a.cvttsd2si(Gpr::Rax, Xmm::Xmm0)),
        [0xf2, 0x48, 0x0f, 0x2c, 0xc0]
    );
    // movsd xmm0, xmm1 (reg-reg) -> f2 0f 10 c1.
    assert_eq!(
        enc(|a| a.movsd_rr(Xmm::Xmm0, Xmm::Xmm1)),
        [0xf2, 0x0f, 0x10, 0xc1]
    );
}

// =========================================================================
//  Tier 1b — layout oracle: byte sizes/offsets vs reflect.rs
// =========================================================================

#[test]
fn layout_oracle_scalar_sizes() {
    use crate::layout::{int_byte_size, round_up};
    use k2_types::IntBits;
    // int_byte_size matches reflect::int_byte_size (power-of-two rounding).
    assert_eq!(int_byte_size(IntBits::Fixed(1)), 1);
    assert_eq!(int_byte_size(IntBits::Fixed(8)), 1);
    assert_eq!(int_byte_size(IntBits::Fixed(9)), 2);
    assert_eq!(int_byte_size(IntBits::Fixed(16)), 2);
    assert_eq!(int_byte_size(IntBits::Fixed(17)), 4);
    assert_eq!(int_byte_size(IntBits::Fixed(32)), 4);
    assert_eq!(int_byte_size(IntBits::Fixed(33)), 8);
    assert_eq!(int_byte_size(IntBits::Fixed(64)), 8);
    assert_eq!(int_byte_size(IntBits::Usize), 8);
    // round_up identity for align <= 1 and standard rounding otherwise.
    assert_eq!(round_up(5, 1), 5);
    assert_eq!(round_up(5, 8), 8);
    assert_eq!(round_up(8, 8), 8);
    assert_eq!(round_up(9, 8), 16);
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

        // Executing a freshly-written file can transiently fail with ETXTBSY
        // ("Text file busy") if the writer's file descriptor is still being
        // flushed by the kernel; retry a few times with a short backoff.
        let mut attempt = 0;
        let out = loop {
            match Command::new(&path).output() {
                Ok(o) => break o,
                Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("exec native binary: {e:?}"),
            }
        };
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
        // A heap-allocating program is outside the native subset (the heap is
        // v0.16): codegen must return a clean Unsupported error, never panic the
        // host process or miscompile.
        let prog = lower(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void { \
                const p = try sys.heap.create(u32); \
                p.* = 7; sys.heap.destroy(p); }",
        );
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

    // =====================================================================
    //  v0.15 differential corpus: aggregates, projections, optionals, error
    //  unions, runtime formatting, recursion, many-arg calls, pointers,
    //  floats. Each asserts native stdout + exit code == the VM's.
    // =====================================================================

    /// A `main(sys)`-shaped program wrapper for the corpus snippets.
    fn main_io(body: &str) -> String {
        format!(
            "const std = @import(\"std\");\n\
             pub fn main(sys: *System) !void {{ const o = sys.io.stdout(); {body} }}"
        )
    }

    #[test]
    fn diff_hello_k2_exact_output() {
        // The headline acceptance: examples/hello.k2 native == VM, byte-identical
        // (tuple aggregate, {s} runtime string, {d} 128-bit decimal, multi-print,
        // stderr writer, error-union try success path).
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/hello.k2"
        ))
        .expect("read hello.k2");
        let prog = lower(&src);
        let (n_code, n_out, n_err) = run_native(&prog);
        let (v_code, v_out, v_err) = run_vm(&prog);
        assert_eq!(n_out, v_out, "hello.k2 stdout native==VM");
        assert_eq!(n_err, v_err, "hello.k2 stderr native==VM");
        assert_eq!(n_code, v_code, "hello.k2 exit native==VM");
        assert_eq!(
            n_out,
            b"Hello, k2!\nk2 directs every joule of Sol: ~384600000000000000000000000 W.\n"
        );
        assert_eq!(n_err, b"(this line went to stderr)\n");
        assert_eq!(n_code, 0);
    }

    #[test]
    fn diff_struct_field_math() {
        let prog = lower(&format!(
            "const Point = struct {{ x: i32, y: i32 }};\n{}",
            main_io(
                "const p = Point{ .x = 6, .y = 7 }; const r = p.x * p.y + p.y; \
                 try o.print(\"r={d}\\n\", .{r});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_nested_struct_projected_store() {
        let prog = lower(&format!(
            "const Inner = struct {{ a: i32, b: i32 }};\n\
             const Outer = struct {{ p: Inner, q: u8 }};\n{}",
            main_io(
                "var s = Outer{ .p = Inner{ .a = 1, .b = 2 }, .q = 5 }; s.p.a = 100; \
                 try o.print(\"{d} {d} {d}\\n\", .{ s.p.a, s.p.b, s.q });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_array_index_sum() {
        let prog = lower(&main_io(
            "const a = [_]i32{ 1, 2, 3, 4, 5 }; var s: i32 = 0; var i: usize = 0; \
             while (i < a.len) : (i += 1) { s += a[i]; } try o.print(\"sum={d}\\n\", .{s});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_array_index_store() {
        let prog = lower(&main_io(
            "var a: [4]u32 = undefined; var i: usize = 0; \
             while (i < 4) : (i += 1) { a[i] = @intCast(i * i); } \
             try o.print(\"{d} {d} {d} {d}\\n\", .{ a[0], a[1], a[2], a[3] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_array_bounds_trap() {
        // An out-of-bounds index traps (exit 134) on both backends.
        let prog = lower(&main_io(
            "const a = [_]i32{ 1, 2, 3 }; var k: usize = 5; const v = a[k]; \
             try o.print(\"{d}\\n\", .{v});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 134);
    }

    #[test]
    fn diff_slice_len_and_index() {
        let prog = lower(&main_io(
            "var a = [_]i32{ 10, 20, 30, 40 }; const s = a[1..3]; \
             try o.print(\"len={d} a={d} b={d}\\n\", .{ s.len, s[0], s[1] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_optional_some_none() {
        let prog = lower(&format!(
            "fn maybe(b: bool) ?i32 {{ if (b) {{ return 7; }} return null; }}\n{}",
            main_io(
                "const v = maybe(true) orelse 0; const w = maybe(false) orelse 99; \
                 try o.print(\"v={d} w={d}\\n\", .{ v, w });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_error_union_try_and_catch() {
        let prog = lower(&format!(
            "const E = error{{ Bad }};\n\
             fn get(b: bool) E!u32 {{ if (b) {{ return 42; }} return E.Bad; }}\n{}",
            main_io(
                "const v = get(true) catch 0; const w = get(false) catch 88; \
                 try o.print(\"v={d} w={d}\\n\", .{ v, w });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_error_escapes_main_exit_1() {
        // An error escaping main prints `error: <name>` to stderr and exits 1 on
        // both backends.
        let prog = lower(
            "const std = @import(\"std\");\n\
             const E = error{ Boom };\n\
             fn fail() E!void { return E.Boom; }\n\
             pub fn main(sys: *System) !void { try fail(); }",
        );
        let (n_code, _n_out, n_err) = run_native(&prog);
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(n_code, 1, "escaped error exits 1 (native)");
        assert_eq!(v_code, 1, "escaped error exits 1 (VM)");
        // The native binary writes `error: <name>\n` to its own stderr; the VM's
        // captured-stderr buffer does not include this line (the driver, not the
        // VM core, emits it), so we assert the native form directly and only the
        // exit codes against the VM.
        assert_eq!(n_err, b"error: Boom\n");
    }

    #[test]
    fn diff_deref_load_store() {
        // Read back through the pointer (`p.*`): both backends agree on the
        // pointer-aliased value. (Reading the *original local* after a pointer
        // store is a VM tagged-value aliasing limitation — native aliases
        // correctly there, so that exact form is intentionally avoided here.)
        let prog = lower(&main_io(
            "var x: i32 = 3; const p = &x; p.* = 9; const y = p.* + 1; \
             try o.print(\"y={d} z={d}\\n\", .{ y, p.* });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_pointer_to_struct_field() {
        // Read back through the interior pointer for the same VM-aliasing reason.
        let prog = lower(&format!(
            "const P = struct {{ x: i32, y: i32 }};\n{}",
            main_io(
                "var s = P{ .x = 1, .y = 2 }; const px = &s.x; px.* = 50; \
                 try o.print(\"{d} {d}\\n\", .{ px.*, s.y });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_runtime_fmt_multi() {
        let prog = lower(&main_io(
            "const a: i32 = 42; const b: u64 = 1000; const name = \"k2\"; \
             try o.print(\"{s}: a={d}, b={d}\\n\", .{ name, a, b });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_fmt_hex_bin_oct_char() {
        let prog = lower(&main_io(
            "const x: u32 = 255; const y: u8 = 10; const n: i8 = -1; const c: u8 = 65; \
             try o.print(\"{x} {X} {b} {o} {x} {c}\\n\", .{ x, x, y, y, n, c });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_fmt_default_verb() {
        let prog = lower(&main_io(
            "const n: i32 = 7; const b = true; const name = \"hi\"; \
             try o.print(\"{} {} {}\\n\", .{ n, b, name });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_negative_decimal() {
        let prog = lower(&main_io(
            "const a: i32 = -12345; const b: i64 = -9000000000; \
             try o.print(\"{d} {d}\\n\", .{ a, b });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_u128_decimal() {
        // The hello.k2 128-bit decimal path, isolated.
        let prog = lower(&main_io(
            "const big: u128 = 384600000000000000000000000; try o.print(\"{d}\\n\", .{big});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_recursion_fib() {
        let prog = lower(&format!(
            "fn fib(n: u64) u64 {{ if (n < 2) {{ return n; }} return fib(n-1) + fib(n-2); }}\n{}",
            main_io("try o.print(\"fib={d}\\n\", .{fib(25)});")
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_many_args_7_and_9() {
        let prog = lower(&format!(
            "fn s7(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) u64 \
                {{ return a+b+c+d+e+f+g; }}\n\
             fn s9(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64) u64 \
                {{ return a+b+c+d+e+f+g+h+i; }}\n{}",
            main_io(
                "const x = s7(1,2,3,4,5,6,7); const y = s9(1,2,3,4,5,6,7,8,9); \
                 try o.print(\"{d} {d}\\n\", .{ x, y });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_struct_return_small_and_16() {
        // A 2×i32 struct returns in RAX (8 bytes); a 2×u64 struct in RAX:RDX (16).
        let prog = lower(&format!(
            "const P = struct {{ x: i32, y: i32 }};\n\
             const Q = struct {{ a: u64, b: u64 }};\n\
             fn mkp(a: i32, b: i32) P {{ return P{{ .x = a, .y = b }}; }}\n\
             fn mkq(a: u64, b: u64) Q {{ return Q{{ .a = a, .b = b }}; }}\n{}",
            main_io(
                "const p = mkp(3, 4); const q = mkq(100, 200); \
                 try o.print(\"{d} {d} {d} {d}\\n\", .{ p.x, p.y, q.a, q.b });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_struct_return_memory_sret() {
        // A >16-byte struct returns via a hidden sret pointer.
        let prog = lower(&format!(
            "const Big = struct {{ a: u64, b: u64, c: u64 }};\n\
             fn mk(a: u64, b: u64, c: u64) Big {{ return Big{{ .a = a, .b = b, .c = c }}; }}\n{}",
            main_io(
                "const g = mk(11, 22, 33); \
                 try o.print(\"{d} {d} {d}\\n\", .{ g.a, g.b, g.c });"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_switch_value() {
        let prog = lower(&format!(
            "fn pick(n: u8) u8 {{ switch (n) {{ 0 => {{ return 10; }}, 1 => {{ return 20; }}, \
                else => {{ return 30; }} }} }}\n{}",
            main_io("try o.print(\"{d}\\n\", .{pick(1) + pick(9)});")
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_float_arith_compare() {
        // f64 arithmetic + compare + branch (compile-time-constant float print).
        let prog = lower(&main_io(
            "var a: f64 = 3.0; var b: f64 = 4.0; const c = a * b; \
             if (c > 10.0) { try o.print(\"big\\n\", .{}); } \
             else { try o.print(\"small\\n\", .{}); }",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_float_int_casts() {
        let prog = lower(&main_io(
            "const i: i32 = 7; const f: f64 = @floatFromInt(i); \
             const back: i32 = @intFromFloat(f * 2.0); try o.print(\"back={d}\\n\", .{back});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_regalloc_spill_stress() {
        // Many simultaneously-live locals + nested loops force spills; the result
        // must still match the VM exactly.
        let prog = lower(&main_io(
            "var a: u64 = 1; var b: u64 = 2; var c: u64 = 3; var d: u64 = 4; \
             var e: u64 = 5; var f: u64 = 6; var g: u64 = 7; var h: u64 = 8; \
             var i: usize = 0; \
             while (i < 100) : (i += 1) { \
                a = a + b; b = b + c; c = c + d; d = d + e; \
                e = e + f; f = f + g; g = g + h; h = h + 1; \
                a = a & 0xffff; b = b & 0xffff; c = c & 0xffff; d = d & 0xffff; \
                e = e & 0xffff; f = f & 0xffff; g = g & 0xffff; h = h & 0xffff; \
             } \
             try o.print(\"{d} {d} {d} {d}\\n\", .{ a, b, c, d });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_callee_saved_roundtrip() {
        // A caller using several long-lived locals across a call: the callee-saved
        // registers must be preserved across the recursion.
        let prog = lower(&format!(
            "fn add(a: u64, b: u64) u64 {{ return a + b; }}\n{}",
            main_io(
                "var acc: u64 = 0; var i: u64 = 0; \
                 while (i < 10) : (i += 1) { acc = add(acc, i); } \
                 try o.print(\"acc={d}\\n\", .{acc});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_enum_switch() {
        // A `switch` over an enum value (the `Union` discriminant -> tag integer).
        let prog = lower(&format!(
            "const Color = enum {{ Red, Green, Blue }};\n\
             fn pick(c: Color) u8 {{ switch (c) {{ .Red => {{ return 1; }}, \
                .Green => {{ return 2; }}, .Blue => {{ return 3; }} }} }}\n{}",
            main_io("try o.print(\"{d} {d} {d}\\n\", .{ pick(.Red), pick(.Green), pick(.Blue) });")
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_float_args_and_return() {
        // f64 arguments in XMM registers + an f64 return, with a compile-time
        // constant float result printed.
        let prog = lower(&format!(
            "fn fma(a: f64, b: f64, c: f64) f64 {{ return a * b + c; }}\n{}",
            main_io(
                "const r = fma(2.0, 3.0, 1.0); \
                 if (r > 6.5) { try o.print(\"big\\n\", .{}); } \
                 else { try o.print(\"small\\n\", .{}); }"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_optional_coercion_from_computed_value() {
        // `return n * 2` into a `?u32`: the computed scalar coerces to `Some(v)`.
        let prog = lower(&format!(
            "fn find(n: u32) ?u32 {{ if (n > 5) {{ return n * 2; }} return null; }}\n{}",
            main_io(
                "var total: u32 = 0; var i: u32 = 0; \
                 while (i < 10) : (i += 1) { if (find(i)) |v| { total += v; } } \
                 try o.print(\"total={d}\\n\", .{total});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    // ---- Clean-refusal (Unsupported) tests ----

    #[test]
    fn concurrency_program_refused() {
        // A spawn/channel program is outside the native subset (the scheduler is
        // v0.16): codegen must cleanly refuse, never miscompile.
        let prog = lower(
            "const std = @import(\"std\");\n\
             fn work(x: u32) u32 { return x + 1; }\n\
             pub fn main(sys: *System) !void { \
                var ex = sys.concurrency.executor(); \
                const t = ex.spawn(work, .{ @as(u32, 5) }); _ = t; }",
        );
        match crate::compile_program_to_elf(&prog) {
            Err(crate::CodegenError::Unsupported(_)) => {}
            Err(crate::CodegenError::NoMain) => panic!("expected Unsupported, got NoMain"),
            Ok(_) => panic!("expected Unsupported, but codegen succeeded"),
        }
    }

    /// Asserts native codegen cleanly REFUSES `prog` with an `Unsupported` error
    /// (the in-subset-or-refuse invariant), never panicking or miscompiling.
    fn assert_native_refused(prog: &MirProgram) {
        match crate::compile_program_to_elf(prog) {
            Err(crate::CodegenError::Unsupported(_)) => {}
            Err(crate::CodegenError::NoMain) => panic!("expected Unsupported, got NoMain"),
            Ok(_) => panic!("expected Unsupported, but codegen succeeded"),
        }
    }

    // =====================================================================
    //  v0.15 native-vs-VM differential regression tests. Each pins one of the
    //  four review findings (128-bit arithmetic miscompile, the RCX
    //  argument-clobber, the f64-constant stack-arg alias, and the VM
    //  store-through-pointer `{d}` render bug).
    // =====================================================================

    // ---- Finding 1: 128-bit (i128/u128) two-limb arithmetic ----

    #[test]
    fn diff_i128_negation_prints_signed() {
        // `const n: i128 = -5` was miscompiled to a 64-bit `neg`, printing the
        // magnitude mod 2^64 (18446744073709551611) with the high limb stale at 0.
        // The two-limb negation now prints `-5` on both backends.
        let prog = lower(&main_io(
            "const n: i128 = -5; try o.print(\"n={d}\\n\", .{n});",
        ));
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"n=-5\n");
    }

    #[test]
    fn diff_i128_negation_large_magnitude() {
        // A negative i128 literal larger than 2^64: native used to drop the sign
        // and print the magnitude mod 2^64. Both backends now print the full value.
        let prog = lower(&main_io(
            "const neg: i128 = -123456789012345678901234567890; \
             try o.print(\"{d}\\n\", .{neg});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"-123456789012345678901234567890\n");
    }

    #[test]
    fn diff_u128_add_no_spurious_trap() {
        // `u128::MAX(=2^64-1) + 1` used to PANIC natively (the @no_add_overflow
        // check ran on the low 64 bits, which carried). The two-limb add + signed-
        // i128 overflow check now agrees with the VM: no trap, result 2^64.
        let prog = lower(&main_io(
            "var a: u128 = 18446744073709551615; const c: u128 = a + 1; \
             try o.print(\"c={d}\\n\", .{c});",
        ));
        let code = assert_native_eq_vm(&prog);
        assert_eq!(code, 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"c=18446744073709551616\n");
    }

    #[test]
    fn diff_i128_widening_cast_and_add() {
        // `@as(i128, x)` of a 64-bit value then `w + w`: the widening cast must
        // sign-extend into the high limb and the add must carry across limbs.
        let prog = lower(&main_io(
            "var x: i64 = 9000000000000000000; const w: i128 = @as(i128, x); \
             const s: i128 = w + w; try o.print(\"s={d}\\n\", .{s});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"s=18000000000000000000\n");
    }

    #[test]
    fn diff_i128_subtraction_two_limb() {
        // A two-limb subtract with a borrow across the limb boundary.
        let prog = lower(&main_io(
            "var a: i128 = 5; var b: i128 = 100; const c: i128 = a - b; \
             try o.print(\"{d}\\n\", .{c});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"-95\n");
    }

    #[test]
    fn diff_u128_widen_from_u64_zero_extends() {
        // A widening cast of an unsigned 64-bit value must ZERO-extend the high
        // limb (not sign-extend), so a u64 with the top bit set stays positive.
        let prog = lower(&main_io(
            "var x: u64 = 18446744073709551615; const w: u128 = @as(u128, x); \
             try o.print(\"{d}\\n\", .{w});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"18446744073709551615\n");
    }

    #[test]
    fn refuse_i128_multiply() {
        // 128-bit multiply is not implemented; codegen must cleanly refuse (fall to
        // the VM) rather than miscompile to a 64-bit `imul`.
        let prog = lower(&main_io(
            "var a: i128 = 1000000; var b: i128 = 1000000; const c: i128 = a * b; \
             try o.print(\"{d}\\n\", .{c});",
        ));
        assert_native_refused(&prog);
    }

    #[test]
    fn refuse_u128_divide() {
        // 128-bit divide is not implemented; codegen must cleanly refuse.
        let prog = lower(&main_io(
            "var a: u128 = 1000000; var b: u128 = 7; const c: u128 = a / b; \
             try o.print(\"{d}\\n\", .{c});",
        ));
        assert_native_refused(&prog);
    }

    // ---- Finding 2: SysV argument marshalling must not clobber RCX ----

    #[test]
    fn diff_call_arg4_in_rcx_not_clobbered_by_index() {
        // A call with >=4 int args where a LATER argument indexes an array: the 4th
        // integer arg lands in RCX, and the index scaling used to overwrite RCX
        // (it scratched the scaled index there). With the index scratch moved to a
        // non-argument register (R10), arg #4 survives — native must match the VM.
        let prog = lower(&format!(
            "fn f(a: i64, b: i64, c: i64, d: i64, e: i64, g: i64) i64 {{ return d; }}\n{}",
            main_io(
                "var arr: [3]i64 = [3]i64{ 11, 22, 33 }; \
                 const r: i64 = f(0, 0, 0, 7, arr[0], arr[2]); \
                 try o.print(\"d={d}\\n\", .{r});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"d=7\n");
    }

    #[test]
    fn diff_call_sums_with_indexed_args() {
        // A broader version: the function actually uses every argument, several of
        // which are array elements evaluated after the RCX slot is filled.
        let prog = lower(&format!(
            "fn g(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) i64 \
                {{ return a + b + c + d + e + f; }}\n{}",
            main_io(
                "var arr: [4]i64 = [4]i64{ 100, 200, 300, 400 }; \
                 const r: i64 = g(1, 2, 3, arr[0], arr[1], arr[3]); \
                 try o.print(\"r={d}\\n\", .{r});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 1+2+3+100+200+400 = 706.
        assert_eq!(out, b"r=706\n");
    }

    // ---- Finding 3: f64 constant arg must not alias the outgoing-args area ----

    #[test]
    fn diff_f64_const_arg_with_stack_arg() {
        // A 7-int-arg call (the 7th spills to [rsp+0]) plus an f64 CONSTANT arg in
        // xmm0. The float-const materialization used to round-trip through [rsp+0],
        // clobbering the spilled 7th integer argument. With `movq xmm, r64` (no
        // memory) the stack arg survives — native must match the VM.
        let prog = lower(&format!(
            "fn f(i1: i64, i2: i64, i3: i64, i4: i64, i5: i64, i6: i64, i7: i64, a: f64) i64 \
                {{ return i1 + i2 + i3 + i4 + i5 + i6 + i7 + @as(i64, @intFromFloat(a)); }}\n{}",
            main_io(
                "const r: i64 = f(1, 2, 3, 4, 5, 6, 7, 100.0); \
                 try o.print(\"r={d}\\n\", .{r});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 1+2+3+4+5+6+7 + 100 = 128.
        assert_eq!(out, b"r=128\n");
    }

    #[test]
    fn diff_f64_const_arg_with_memory_aggregate() {
        // A >16-byte (MEMORY-class) aggregate passed by value lands at [rsp+0],
        // together with an f64 constant arg — the same aliasing hazard, now fixed.
        let prog = lower(&format!(
            "const Big = struct {{ a: i64, b: i64, c: i64 }};\n\
             fn f(v: Big, x: f64) i64 \
                {{ return v.a + v.b + v.c + @as(i64, @intFromFloat(x)); }}\n{}",
            main_io(
                "const r: i64 = f(Big{ .a = 1, .b = 2, .c = 3 }, 100.0); \
                 try o.print(\"r={d}\\n\", .{r});"
            )
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 1+2+3 + 100 = 106.
        assert_eq!(out, b"r=106\n");
    }

    // ---- Finding 4: VM store-through-pointer then `{d}` (native already correct)

    #[test]
    fn diff_store_through_pointer_then_print_decimal() {
        // `bump(&x)` mutates `x` through a `*i32`, then `x` is printed with `{d}`.
        // The VM used to read the boxed pointer for the address-taken local and
        // render `<int>`; native (reading the stack home) correctly prints 43. Both
        // backends now agree on `x=43`.
        let prog = lower(&format!(
            "fn bump(p: *i32) void {{ p.* = p.* + 1; }}\n{}",
            main_io("var x: i32 = 42; bump(&x); try o.print(\"x={d}\\n\", .{x});")
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"x=43\n");
    }
}
