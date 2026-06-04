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
    let img = crate::elf::write_elf(&[0x90], &[], 0);
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
    let img = crate::elf::write_elf(&[0x90, 0x90], b"hi\n", 0);
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
    ///
    /// The MIR optimizer is run at the level the mode selects (Debug -> identity,
    /// ReleaseSafe -> Safe, ReleaseFast -> Fast), exactly like the driver's
    /// native/VM paths. This is the milestone's key change: native lowering now
    /// sees the *optimized* MIR (folded constants, propagated copies, simplified
    /// CFG), so the differential tests exercise the optimizer-on-native path that
    /// the old `OptLevel::None`-only helper could not.
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
        let level = match mode {
            BuildMode::Debug => k2_opt::OptLevel::None,
            BuildMode::ReleaseSafe => k2_opt::OptLevel::Safe,
            BuildMode::ReleaseFast => k2_opt::OptLevel::Fast,
        };
        k2_opt::optimize(&mut prog, level);
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
        // Mirror the `k2c run-native` driver's exit-code convention: a normal exit
        // yields its code, while a signal death yields `128 + signo` (the shell
        // convention) so a SIGSEGV (139) is distinguishable from a clean k2
        // panic-trap (134) — otherwise `ExitStatus::code()` is `None` for a
        // signalled child and the harness would report `-1`, hiding the signal.
        let code = native_exit_code(&out.status);
        (code, out.stdout, out.stderr)
    }

    /// The exit code a native run reports, matching `k2c`'s `native_exit_code`: a
    /// normal exit is its code; a signal-killed child is `128 + signo` (so a
    /// SIGSEGV is `139`, a SIGFPE `136`, etc.).
    fn native_exit_code(st: &std::process::ExitStatus) -> i32 {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = st.code() {
            code
        } else if let Some(signo) = st.signal() {
            128 + signo
        } else {
            134
        }
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

    // =====================================================================
    //  v0.16 — the native `*System` runtime (heap / clock / random / env)
    // =====================================================================

    /// Reads an `examples/*.k2` source file relative to this crate.
    fn read_example(name: &str) -> String {
        let path = format!("{}/../../examples/{}.k2", env!("CARGO_MANIFEST_DIR"), name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn diff_errors_k2_exact_output() {
        // HARD ACCEPTANCE: examples/errors.k2 native == VM (stdout + exit), driven
        // by the native heap (`create`/`destroy` via the mmap allocator), the
        // error-union try/catch/errdefer machinery, `@errorName`, and `{s:>14}`
        // width/alignment formatting.
        let prog = lower(&read_example("errors"));
        let (n_code, n_out, _n_err) = run_native(&prog);
        let (v_code, v_out, _v_err) = run_vm(&prog);
        assert_eq!(n_out, v_out, "errors.k2 stdout native==VM");
        assert_eq!(n_code, v_code, "errors.k2 exit native==VM");
        assert_eq!(n_code, 0);
        assert!(
            String::from_utf8_lossy(&n_out).contains("doubled(\"21\") = 42"),
            "errors.k2 produced its expected first line"
        );
    }

    #[test]
    fn diff_heap_create_destroy_roundtrip() {
        // `create(T)` returns a real heap pointer (mmap-backed); store, deref, free.
        let prog = lower(&main_io(
            "const a = sys.heap; const p: *u64 = try a.create(u64); \
             p.* = 0xDEAD_BEEF; try o.print(\"v={d}\\n\", .{p.*}); a.destroy(p);",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, format!("v={}\n", 0xDEAD_BEEFu64).into_bytes());
    }

    #[test]
    fn diff_heap_alloc_free_roundtrip() {
        // A `[]u64` slice from the default allocator: fill `i*i`, sum, print, free.
        let prog = lower(&main_io(
            "const a = sys.heap; const xs: []u64 = try a.alloc(u64, 16); \
             defer a.free(xs); \
             for (xs, 0..) |*slot, i| { const v: u64 = @intCast(i); slot.* = v * v; } \
             var sum: u64 = 0; for (xs) |x| sum += x; \
             try o.print(\"sum={d} last={d}\\n\", .{ sum, xs[15] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 0^2+..+15^2 = 1240, 15^2 = 225.
        assert_eq!(out, b"sum=1240 last=225\n");
    }

    #[test]
    fn diff_gpa_no_leak_clean() {
        // The GPA's no-leak path: alloc + free everything, `deinit()` reports
        // `false`, no panic, exit 0 — native == VM.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const a = gpa.allocator(); \
             const p: *u32 = try a.create(u32); p.* = 7; \
             try o.print(\"v={d}\\n\", .{p.*}); a.destroy(p); \
             const leaked = gpa.deinit(); \
             try o.print(\"leaked={d}\\n\", .{leaked});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"v=7\nleaked=0\n");
    }

    #[test]
    fn diff_gpa_leak_detected_panics() {
        // The GPA's leak path: an allocation escapes the free (guarded by a
        // runtime-false branch so the static leak checker admits it), so
        // `gpa.deinit()` reports `true` and `@panic` traps (exit 134) — native ==
        // VM. This exercises the runtime leak counter end-to-end.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const a = gpa.allocator(); \
             const p: *u32 = try a.create(u32); p.* = 1; \
             var keep: bool = (p.* == 1); \
             if (not keep) { a.destroy(p); } \
             const leaked = gpa.deinit(); \
             if (leaked) @panic(\"memory leak detected at shutdown\"); \
             try o.print(\"unreached\\n\", .{});",
        ));
        let (n_code, _n_out, _n_err) = run_native(&prog);
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(n_code, v_code, "leak panic exit native==VM");
        assert_eq!(n_code, 134, "a detected leak @panics (exit 134)");
    }

    #[test]
    fn diff_double_free_traps() {
        // Freeing the same GPA pointer twice traps (exit 134) on both backends.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const a = gpa.allocator(); \
             const p: *u32 = try a.create(u32); p.* = 1; \
             a.destroy(p); a.destroy(p); \
             const leaked = gpa.deinit(); _ = leaked; \
             try o.print(\"unreached\\n\", .{});",
        ));
        let (n_code, _n_out, _n_err) = run_native(&prog);
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(v_code, 134, "VM double-free traps");
        assert_eq!(n_code, 134, "native double-free traps == VM");
    }

    #[test]
    fn diff_clock_and_random_are_deterministic() {
        // The deterministic clock (advanced only by sleep) and the reproducible
        // splitmix64 PRNG must produce the exact same values native == VM.
        let prog = lower(&main_io(
            "const t0 = sys.clock.now(); sys.clock.sleep(1000); \
             const t1 = sys.clock.now(); \
             const r0 = sys.random.int(); const r1 = sys.random.int(); \
             try o.print(\"t0={d} t1={d} r0={d} r1={d}\\n\", .{ t0, t1, r0, r1 });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_random_bytes_match_vm() {
        // `sys.random.bytes(&buf)` fills a buffer with the same reproducible bytes.
        let prog = lower(&main_io(
            "var buf: [8]u8 = undefined; sys.random.bytes(&buf); \
             var sum: u32 = 0; for (buf) |b| sum += @intCast(b); \
             try o.print(\"sum={d} b0={d} b7={d}\\n\", .{ sum, buf[0], buf[7] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
    }

    #[test]
    fn diff_env_get_absent() {
        // Offline-absent env: `sys.env.get(...)` yields `null` on both backends.
        let prog = lower(&main_io(
            "const v = sys.env.get(\"PATH\"); \
             const present: u8 = if (v == null) 0 else 1; \
             try o.print(\"present={d}\\n\", .{present});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"present=0\n");
    }

    #[test]
    fn diff_arraylist_u32_growth() {
        // A word-scalar generic container DOES run natively: `ArrayList(u32)` grows
        // its heap backing via `realloc` and the field-slice word stride keeps the
        // shared `deferred`-element methods and the concrete reader in agreement.
        let prog = lower(&main_io(
            "var list = std.ArrayList(u32).init(sys.heap); \
             defer list.deinit(); \
             var i: u32 = 0; while (i < 10) : (i += 1) { try list.append(i * i); } \
             try o.print(\"len={d} a3={d} a9={d}\\n\", \
                .{ list.items.len, list.items[3], list.items[9] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 3*3=9, 9*9=81.
        assert_eq!(out, b"len=10 a3=9 a9=81\n");
    }

    #[test]
    fn refuse_generic_aggregate_container_cleanly() {
        // A generic container that grows a heap slice (`ArrayList`/`List`) is the
        // un-monomorphized case the v0.16 backend cleanly REFUSES (so the program
        // runs on the VM) rather than miscompiling. A generic container of an
        // *aggregate* element (`ArrayList([]const u8)`) cannot ride the shared
        // scalar `deferred` value-parameter ABI losslessly, so it is refused. The
        // refusal is an `Err`, never a panic or a wrong ELF.
        let prog = lower(&main_io(
            "var list = std.ArrayList([]const u8).init(sys.heap); \
             defer list.deinit(); \
             try list.append(\"hello\"); \
             try o.print(\"len={d}\\n\", .{list.items.len});",
        ));
        let r = crate::compile_program_to_elf(&prog);
        assert!(
            matches!(r, Err(crate::CodegenError::Unsupported(_))),
            "a generic aggregate-element heap container must be cleanly refused"
        );
        assert!(r.is_err());
        // And it still runs on the VM.
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(v_code, 0, "the refused program runs fine on the VM");
    }

    // ---------------------------------------------------------------------
    //  v0.16 native-runtime defect regressions (realloc/deinit live-list,
    //  the @allocId registry cap, and full-fidelity use-after-free)
    // ---------------------------------------------------------------------

    #[test]
    fn diff_gpa_realloc_then_deinit_no_segfault() {
        // REGRESSION: a non-null `realloc` under a TRACKED GeneralPurposeAllocator
        // followed by `gpa.deinit()` used to print the correct value then SIGSEGV
        // (native exit 139) while the VM exits 0 — the freed old block was
        // munmap-ed eagerly but left threaded on the slot's `live_head`, so the
        // deinit reclamation walk dereferenced unmapped memory. After the fix the
        // old block is unlinked (and kept mapped) on free, so teardown is clean.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             defer { const l = gpa.deinit(); if (l) @panic(\"leak\"); } \
             const al = gpa.allocator(); \
             var xs: []u64 = try al.alloc(u64, 4); \
             xs = try al.realloc(xs, 10); \
             xs[9] = 99; \
             try o.print(\"xs[9]={d}\\n\", .{xs[9]}); \
             al.free(xs);",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"xs[9]=99\n");
    }

    #[test]
    fn diff_arraylist_u32_growth_on_gpa_deinit() {
        // REGRESSION: the canonical `std.ArrayList` growth + leak-checking GPA
        // pattern. `ArrayList(u32)` on a *GeneralPurposeAllocator* (not the
        // untracked `sys.heap`) grown well past its first `realloc` (multiple
        // reallocs), then `list.deinit()` + `gpa.deinit()`. This is the exact
        // shape that the v0.16 teardown SIGSEGV broke; it must now exit 0
        // native == VM (the old `diff_arraylist_u32_growth` used `sys.heap` and
        // missed the tracked-allocator path entirely).
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             defer { const l = gpa.deinit(); if (l) @panic(\"leak\"); } \
             var list = std.ArrayList(u32).init(gpa.allocator()); \
             defer list.deinit(); \
             var i: u32 = 0; while (i < 40) : (i += 1) { try list.append(i * i); } \
             try o.print(\"len={d} a3={d} a39={d}\\n\", \
                .{ list.items.len, list.items[3], list.items[39] });",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        // 3*3=9, 39*39=1521.
        assert_eq!(out, b"len=40 a3=9 a39=1521\n");
    }

    #[test]
    fn diff_arena_realloc_then_deinit_no_segfault() {
        // REGRESSION: the same dangling-live-list crash on an ArenaAllocator —
        // `alloc` then `realloc` then `arena.deinit()` used to SIGSEGV (139) vs
        // VM 0. The arena realloc-old block is now unlinked from the bulk-free
        // list (mirroring the VM's `retrack_realloc`) before teardown.
        let prog = lower(&main_io(
            "var arena = std.heap.ArenaAllocator.init(sys.heap); \
             const a = arena.allocator(); \
             var xs: []u64 = try a.alloc(u64, 4); \
             xs = try a.realloc(xs, 10); \
             xs[9] = 7; \
             try o.print(\"xs[9]={d}\\n\", .{xs[9]}); \
             arena.deinit();",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"xs[9]=7\n");
    }

    #[test]
    fn diff_gpa_realloc_then_leak_still_detected() {
        // The realloc fix must not weaken leak detection: a `realloc`-ed block left
        // unfreed (guarded by a runtime-false branch so the static leak checker
        // admits it) must still make `gpa.deinit()` report `true` — native == VM.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const al = gpa.allocator(); \
             var xs: []u64 = try al.alloc(u64, 4); \
             xs = try al.realloc(xs, 10); \
             var keep: bool = (xs[0] == 0); \
             if (not keep) { al.free(xs); } \
             const l = gpa.deinit(); \
             try o.print(\"leaked={d}\\n\", .{l});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"leaked=1\n");
    }

    #[test]
    fn diff_double_free_after_realloc_traps() {
        // The realloc fix must not weaken double-free detection: freeing a
        // realloc-ed block twice must trap cleanly (exit 134) — native == VM.
        let prog = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const al = gpa.allocator(); \
             var xs: []u64 = try al.alloc(u64, 4); \
             xs = try al.realloc(xs, 10); \
             al.free(xs); al.free(xs); \
             try o.print(\"unreached\\n\", .{}); \
             _ = gpa.deinit();",
        ));
        let (n_code, _n_out, _n_err) = run_native(&prog);
        let (v_code, _v_out, _v_err) = run_vm(&prog);
        assert_eq!(n_code, v_code, "double-free-after-realloc exit native==VM");
        assert_eq!(n_code, 134, "double free traps (exit 134)");
    }

    #[test]
    fn alloc_id_registry_cap_traps_cleanly() {
        // REGRESSION: `@allocId` minted handles via `reg_next++` with no bound
        // check, so the 256th+ allocator scribbled past the writable state PT_LOAD
        // and eventually SIGSEGV'd (139). A program creating more allocators than
        // the registry holds must now TRAP CLEANLY (a deterministic non-139 refusal
        // with a message) rather than corrupt memory / segfault. (The VM is
        // unbounded; this is a documented native cap, so the exit codes differ —
        // the contract here is "no scribble, no segfault", which we assert.)
        let prog = lower(&main_io(
            "var acc: u64 = 0; var i: u64 = 0; \
             while (i < 400) : (i += 1) { \
                var arena = std.heap.ArenaAllocator.init(sys.heap); \
                const a = arena.allocator(); \
                const p: *u64 = try a.create(u64); p.* = i; acc += p.*; \
                arena.deinit(); } \
             try o.print(\"acc={d}\\n\", .{acc});",
        ));
        let (n_code, _n_out, n_err) = run_native(&prog);
        assert_eq!(
            n_code, 134,
            "exceeding the registry cap traps cleanly (exit 134), never segfaults (139)"
        );
        assert_ne!(n_code, 139, "must not segfault");
        assert!(
            String::from_utf8_lossy(&n_err).contains("too many allocators"),
            "the cap trap reports its cause"
        );
        // The VM, being unbounded, runs the same program to completion.
        let (v_code, v_out, _v_err) = run_vm(&prog);
        assert_eq!(v_code, 0, "the VM has no registry cap");
        assert_eq!(v_out, b"acc=79800\n");
    }

    #[test]
    fn alloc_id_registry_just_under_cap_ok() {
        // The bound must not be over-eager: 250 short-lived allocators (well under
        // REG_MAX = 256) still run to completion, native == VM, exit 0.
        let prog = lower(&main_io(
            "var acc: u64 = 0; var i: u64 = 0; \
             while (i < 250) : (i += 1) { \
                var arena = std.heap.ArenaAllocator.init(sys.heap); \
                const a = arena.allocator(); \
                const p: *u64 = try a.create(u64); p.* = i; acc += p.*; \
                arena.deinit(); } \
             try o.print(\"acc={d}\\n\", .{acc});",
        ));
        assert_eq!(assert_native_eq_vm(&prog), 0);
        let (_c, out, _e) = run_native(&prog);
        assert_eq!(out, b"acc=31125\n");
    }

    #[test]
    fn uaf_read_traps_full_payload_native() {
        // REGRESSION + documented-behavior assertion: a use-after-free read of a
        // freed GPA payload must now FAULT for any block size and any offset (the
        // payload is page-isolated and mprotected PROT_NONE in full). Previously a
        // read of the first payload page (e.g. `xs[0]`) returned stale data with
        // exit 0; now it traps. The native fault maps to 139 and the VM's clean
        // `use after free` panic to 134 (the documented exit-code narrowing) — both
        // die, neither returns stale data with exit 0.
        //
        // Small block (4 u32): the historically-untrapped case.
        let small = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const al = gpa.allocator(); \
             var xs: []u32 = try al.alloc(u32, 4); \
             xs[0] = 48; al.free(xs); \
             try o.print(\"v={d}\\n\", .{xs[0]}); \
             const l = gpa.deinit(); _ = l;",
        ));
        let (n_small, _no, _ne) = run_native(&small);
        let (v_small, _vo, _ve) = run_vm(&small);
        assert_eq!(n_small, 139, "native UAF on a small block faults (139)");
        assert_eq!(v_small, 134, "VM UAF panics (134)");

        // Multi-page block (2000 u64), read at offset 0 — the case the old
        // narrowing left readable.
        let big = lower(&main_io(
            "var gpa = std.heap.GeneralPurposeAllocator.init(sys); \
             const al = gpa.allocator(); \
             var xs: []u64 = try al.alloc(u64, 2000); \
             xs[0] = 5; al.free(xs); \
             try o.print(\"v={d}\\n\", .{xs[0]}); \
             const l = gpa.deinit(); _ = l;",
        ));
        let (n_big, _no, _ne) = run_native(&big);
        let (v_big, _vo, _ve) = run_vm(&big);
        assert_eq!(n_big, 139, "native UAF on a multi-page block faults (139)");
        assert_eq!(v_big, 134, "VM UAF panics (134)");
    }

    // =====================================================================
    //  v0.17 — the optimizer-on-native differential, peephole, and speedup
    // =====================================================================

    /// The committed example + bench sources, compiled through the same
    /// std-injecting front end as every other exec test. These are full programs
    /// (`@import("std")`), so `lower_mode` rewrites the std import and injects the
    /// prelude. Each is in the native subset.
    fn v017_corpus() -> Vec<(&'static str, &'static str)> {
        vec![
            ("hello", include_str!("../../../examples/hello.k2")),
            ("errors", include_str!("../../../examples/errors.k2")),
            (
                "allocators",
                include_str!("../../../examples/allocators.k2"),
            ),
            (
                "fib_rec",
                include_str!("../../k2c/bench/bench_fib_rec_native.k2"),
            ),
            (
                "loop_sum",
                include_str!("../../k2c/bench/bench_loop_sum.k2"),
            ),
            (
                "struct_kernel",
                include_str!("../../k2c/bench/bench_struct_kernel.k2"),
            ),
        ]
    }

    /// **The headline differential: native-opt == native-unopt == VM.**
    ///
    /// For every corpus program and every mode, the native backend and the VM must
    /// agree on stdout + exit (`assert_native_eq_vm`), AND the optimized
    /// ReleaseFast native run must match the unoptimized Debug native run (the
    /// optimizer is behavior-preserving). This is the regression that proves Fix A:
    /// before it, `hello` in release modes errored ("non-scalar constant Str"); the
    /// old `OptLevel::None`-only helper could not see it.
    #[test]
    fn diff_native_opt_eq_unopt_eq_vm() {
        for (name, src) in v017_corpus() {
            // native == VM in each mode.
            let dbg = lower_mode(src, BuildMode::Debug);
            let safe = lower_mode(src, BuildMode::ReleaseSafe);
            let fast = lower_mode(src, BuildMode::ReleaseFast);
            assert_native_eq_vm(&dbg);
            assert_native_eq_vm(&safe);
            assert_native_eq_vm(&fast);

            // native-opt (ReleaseFast) == native-unopt (Debug): the optimizer + the
            // peephole are behavior-preserving. (These compute kernels do not trap,
            // so Debug and ReleaseFast produce identical observable output.)
            let (d_code, d_out, _) = run_native(&dbg);
            let (f_code, f_out, _) = run_native(&fast);
            assert_eq!(
                String::from_utf8_lossy(&d_out),
                String::from_utf8_lossy(&f_out),
                "[{name}] native ReleaseFast stdout must equal native Debug"
            );
            assert_eq!(
                d_code, f_code,
                "[{name}] native ReleaseFast exit must equal native Debug"
            );
        }
    }

    /// The peephole pass must measurably shrink `.text` without changing behavior.
    /// We assert a strict reduction on a kernel known to contain the targeted
    /// patterns (the loop kernel: regalloc copies + a fall-through back-edge), a
    /// non-increase everywhere, and — the correctness half — that the peepholed
    /// binary behaves identically to the un-peepholed one for every corpus program.
    #[test]
    fn peephole_shrinks_code_and_preserves_behavior() {
        let mut any_strict = false;
        for (name, src) in v017_corpus() {
            // Compile in both Debug and ReleaseFast; the peephole runs on both.
            for mode in [BuildMode::Debug, BuildMode::ReleaseFast] {
                let prog = lower_mode(src, mode);
                let (img_on, stats) = crate::compile_program_to_elf_stats(&prog).expect("codegen");
                assert!(
                    stats.text_bytes_after <= stats.text_bytes_before,
                    "[{name}/{mode:?}] peephole must never grow .text \
                     ({} -> {})",
                    stats.text_bytes_before,
                    stats.text_bytes_after
                );
                if stats.text_bytes_after < stats.text_bytes_before {
                    any_strict = true;
                }

                // Correctness: the peephole-on image (the shipped one) must behave
                // exactly like the peephole-off image. Run both and compare.
                let img_off = crate::encode::with_peephole(false, || {
                    crate::compile_program_to_elf(&prog).expect("codegen-off")
                });
                let (c_on, o_on, _) = run_image(&img_on);
                let (c_off, o_off, _) = run_image(&img_off);
                assert_eq!(o_on, o_off, "[{name}/{mode:?}] peephole changed stdout");
                assert_eq!(c_on, c_off, "[{name}/{mode:?}] peephole changed exit code");
            }
        }
        assert!(
            any_strict,
            "the peephole must strictly shrink .text on at least one corpus program"
        );
    }

    /// Writes an ELF image to a temp file, executes it, and returns
    /// `(exit, stdout, stderr)`. Shares the ETXTBSY retry with [`run_native`].
    fn run_image(img: &crate::ElfImage) -> (i32, Vec<u8>, Vec<u8>) {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1_000_000);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("k2_peep_{}_{}", std::process::id(), n));
        std::fs::write(&path, &img.bytes).expect("write elf");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        let mut attempt = 0;
        let out = loop {
            match Command::new(&path).output() {
                Ok(o) => break o,
                Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("exec image: {e:?}"),
            }
        };
        let _ = std::fs::remove_file(&path);
        (native_exit_code(&out.status), out.stdout, out.stderr)
    }

    /// **Native is much faster than the VM** — a non-flaky, conservative
    /// lower-bound assertion. fib(26) is timed native (min-of-N process exec) vs
    /// the VM (min-of-N in-process), and native must be at least 5x faster. The
    /// measured margin is enormous (the VM is a bytecode interpreter), so a 5x
    /// floor cannot flake even on a slow/loaded CI box; a debug-profile test build
    /// makes the VM *slower*, enlarging the ratio further. Output is asserted
    /// identical native vs VM as an independent correctness gate.
    #[test]
    fn native_is_much_faster_than_vm() {
        let src = include_str!("../../k2c/bench/bench_fib_rec_native.k2");
        let prog = lower_mode(src, BuildMode::ReleaseFast);

        // Build the ELF once; write + exec it best-of-N.
        let img = crate::compile_program_to_elf(&prog).expect("codegen");
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("k2_speedup_{}", std::process::id()));
        std::fs::write(&path, &img.bytes).expect("write");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        const REPS: u32 = 5;
        let mut native_min = u128::MAX;
        let mut native_out = Vec::new();
        let mut native_code = 0;
        for _ in 0..REPS {
            let mut attempt = 0;
            let (us, out) = loop {
                let t0 = std::time::Instant::now();
                match std::process::Command::new(&path).output() {
                    Ok(o) => {
                        native_code = native_exit_code(&o.status);
                        break (t0.elapsed().as_micros(), o.stdout);
                    }
                    Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("exec: {e:?}"),
                }
            };
            native_min = native_min.min(us);
            native_out = out;
        }
        let _ = std::fs::remove_file(&path);

        // The VM, best-of-N in-process on the same optimized MIR.
        let (vm_code, vm_out, _e) = run_vm(&prog);
        let mut vm_min = u128::MAX;
        for _ in 0..REPS {
            let t0 = std::time::Instant::now();
            let _ = k2_vm::run_captured(&prog, k2_vm::RunArgs::new(prog.mode));
            vm_min = vm_min.min(t0.elapsed().as_micros());
        }

        // Correctness gate (independent of timing).
        assert_eq!(
            String::from_utf8_lossy(&native_out),
            String::from_utf8_lossy(&vm_out),
            "native and VM must produce identical output"
        );
        assert_eq!(native_code, vm_code, "native and VM exit must agree");

        // Informational: print the real measured ratio (never a pass/fail input).
        let ratio = vm_min as f64 / native_min.max(1) as f64;
        eprintln!(
            "[native_is_much_faster_than_vm] native={}us vm={}us ratio={:.1}x",
            native_min, vm_min, ratio
        );

        // The non-flaky floor: VM must be at least 5x slower than native.
        assert!(
            vm_min >= 5 * native_min.max(1),
            "expected native >= 5x faster; got native={native_min}us vm={vm_min}us \
             ({ratio:.1}x)"
        );
    }

    /// ReleaseFast strips a safety check that traps in Debug, and native matches the
    /// VM in each mode. A `u8` overflow traps (exit 134) in Debug on both backends;
    /// in ReleaseFast it wraps (no trap, exit 0) on both. This proves the native
    /// check-stripping is correct and mode-for-mode identical to the VM.
    #[test]
    fn releasefast_strips_native_traps() {
        let src = "pub fn main() u8 { var x: u8 = 255; x = x + 1; return x; }";

        // Debug: the overflow check traps on both backends (panic-trap exit 134).
        let dbg = lower_mode(src, BuildMode::Debug);
        let (n_dbg, _o, _e) = run_native(&dbg);
        let (v_dbg, _vo, _ve) = run_vm(&dbg);
        assert_eq!(n_dbg, v_dbg, "Debug: native trap must match the VM");
        assert_eq!(n_dbg, 134, "Debug: u8 overflow traps (134)");

        // ReleaseFast: the check is stripped at lowering; the add wraps to 0, no
        // trap, exit 0 — identical on both backends.
        let fast = lower_mode(src, BuildMode::ReleaseFast);
        let (n_fast, _o, _e) = run_native(&fast);
        let (v_fast, _vo, _ve) = run_vm(&fast);
        assert_eq!(
            n_fast, v_fast,
            "ReleaseFast: native no-trap must match the VM"
        );
        assert_eq!(n_fast, 0, "ReleaseFast: 255+1 wraps to 0, exit 0");

        // The modes differ (the whole point of the strip): Debug traps, Fast does
        // not — identically on both backends.
        assert_ne!(n_dbg, n_fast, "Debug must trap where ReleaseFast does not");
    }

    // =====================================================================
    //  v0.17 review fixes — differential regressions (native == VM, gated
    //  x86_64-linux exec). Each pins one finding.
    // =====================================================================

    /// **Finding #1 — `for (slice) |x|` value capture.** A `for` loop over a slice
    /// PARAMETER (a `&array` coerced to `[]const u32` at the call site) must compute
    /// the real sum on native, matching the VM, in every mode. The bug: `&array`
    /// was marshalled as a single pointer into a `[]T` parameter, so the callee read
    /// a garbage `.len` and the loop summed to 0. The MIR now emits a `MakeSlice`
    /// coercion, so both backends see a fat `{ptr, len}` slice.
    #[test]
    fn diff_for_over_slice_value_capture() {
        let src = "fn sumArr(xs: []const u32) u32 { var t: u32 = 0; \
                   for (xs) |x| { t = t + x; } return t; } \
                   pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                   const a = [_]u32{ 10, 20, 30, 40 }; \
                   try o.print(\"sum={d}\\n\", .{sumArr(&a)}); }";
        for mode in [
            BuildMode::Debug,
            BuildMode::ReleaseSafe,
            BuildMode::ReleaseFast,
        ] {
            let prog = lower_mode(src, mode);
            let (n_code, n_out, _) = run_native(&prog);
            let (v_code, v_out, _) = run_vm(&prog);
            assert_eq!(
                String::from_utf8_lossy(&n_out),
                "sum=100\n",
                "[{mode:?}] for-over-slice value capture must sum to 100 on native"
            );
            assert_eq!(n_out, v_out, "[{mode:?}] for-over-slice native==VM stdout");
            assert_eq!(n_code, v_code, "[{mode:?}] for-over-slice native==VM exit");
        }
    }

    /// **Finding #1 (companion) — `for (array) |x|` and `for (xs, 0..) |x, i|`.** The
    /// by-value capture over a local array, and the 2-operand value+index form,
    /// must also match the VM in every mode.
    #[test]
    fn diff_for_over_array_and_index_forms() {
        // for over a local array, by value.
        let arr = "pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                   const a = [_]u32{ 10, 20, 30, 40 }; var t: u32 = 0; \
                   for (a) |x| { t = t + x; } try o.print(\"a={d}\\n\", .{t}); }";
        // for over a slice with an index capture: sum x*i.
        let idx = "fn weighted(xs: []const u32) u32 { var t: u32 = 0; \
                   for (xs, 0..) |x, i| { t = t + x * @as(u32, @intCast(i)); } return t; } \
                   pub fn main(sys: *System) !void { const o = sys.io.stdout(); \
                   const a = [_]u32{ 10, 20, 30, 40 }; \
                   try o.print(\"w={d}\\n\", .{weighted(&a)}); }";
        for src in [arr, idx] {
            for mode in [
                BuildMode::Debug,
                BuildMode::ReleaseSafe,
                BuildMode::ReleaseFast,
            ] {
                let prog = lower_mode(src, mode);
                let (n_code, n_out, _) = run_native(&prog);
                let (v_code, v_out, _) = run_vm(&prog);
                assert_eq!(n_out, v_out, "[{mode:?}] for-loop native==VM stdout");
                assert_eq!(n_code, v_code, "[{mode:?}] for-loop native==VM exit");
            }
        }
    }

    /// **Finding #2 — const-folded integer print across modes + verbs.** A negative
    /// (comptime-typed) integer constant, and a const-foldable integer expression,
    /// printed with every integer verb, must produce byte-identical native output in
    /// Debug, ReleaseSafe, and ReleaseFast — and match the VM. Before the fix the
    /// optimizer folded the value to a `comptime_int` Const the native print
    /// formatter rejected in release modes (exit 1 "non-integer field"), an
    /// opt-vs-unopt native divergence.
    #[test]
    fn diff_const_folded_int_print_all_verbs() {
        // The negative-constant headline repro, plus one const-foldable expression
        // per verb (the fold collapses the arithmetic to an inline Const).
        let progs = [
            "const c: i64 = -7; try o.print(\"{d}\\n\", .{c});",
            "const c: i64 = -7; try o.print(\"{}\\n\", .{c});",
            "const c: u32 = (32 - 15); try o.print(\"{x}\\n\", .{c});",
            "const c: u32 = (1 << 6) | 1; try o.print(\"{b}\\n\", .{c});",
            "const c: u32 = 8 * 8 + 1; try o.print(\"{o}\\n\", .{c});",
            "const c: u8 = 64 + 1; try o.print(\"{c}\\n\", .{c});",
        ];
        for body in progs {
            let src = main_io(body);
            // The Debug (unopt) native output is the reference; every release mode
            // and the VM must equal it.
            let dbg = lower_mode(&src, BuildMode::Debug);
            let (ref_code, ref_out, _) = run_native(&dbg);
            for mode in [BuildMode::ReleaseSafe, BuildMode::ReleaseFast] {
                let prog = lower_mode(&src, mode);
                let (n_code, n_out, _) = run_native(&prog);
                assert_eq!(
                    n_out, ref_out,
                    "[{mode:?}] `{body}` native release output must equal Debug native"
                );
                assert_eq!(n_code, 0, "[{mode:?}] `{body}` must succeed (exit 0)");
            }
            let (v_code, v_out, _) = run_vm(&dbg);
            assert_eq!(v_out, ref_out, "`{body}` VM output must equal native Debug");
            assert_eq!(v_code, ref_code, "`{body}` VM exit must equal native Debug");
        }
    }

    /// **Finding #4 — ReleaseFast bounds-strip parity.** An out-of-bounds index in
    /// ReleaseFast must not trap on EITHER backend (the bounds check is stripped on
    /// both): exit 0, no panic. (The exact OOB value is UB and need not match; the
    /// observable trap/exit behavior must.) In Debug both still trap (134).
    #[test]
    fn diff_releasefast_strips_bounds_native_eq_vm() {
        let src = main_io(
            "const a = [_]u32{ 10, 20, 30, 40 }; var i: usize = 9; \
             const x = a[i]; try o.print(\"x={d}\\n\", .{x});",
        );
        // Debug: both trap.
        let dbg = lower_mode(&src, BuildMode::Debug);
        let (n_dbg, _, _) = run_native(&dbg);
        let (v_dbg, _, _) = run_vm(&dbg);
        assert_eq!(n_dbg, 134, "Debug OOB traps on native (134)");
        assert_eq!(v_dbg, 134, "Debug OOB traps on the VM (134)");
        // ReleaseFast: neither traps — both exit 0, no panic.
        let fast = lower_mode(&src, BuildMode::ReleaseFast);
        let (n_fast, _, _) = run_native(&fast);
        let (v_fast, _, _) = run_vm(&fast);
        assert_eq!(n_fast, 0, "ReleaseFast native OOB does not trap (exit 0)");
        assert_eq!(v_fast, 0, "ReleaseFast VM OOB does not trap (exit 0)");
    }

    /// **Finding #5 — trap message text native == VM.** The two trap-message tables
    /// (native `trap_message`, VM `PanicInfo::message`) are aligned, so a trap
    /// prints identical text on both backends. Verified for negation overflow and a
    /// narrowing-cast trap (the two the finding called out).
    ///
    /// The native backend writes `panic: <message>\n` to fd 2 (its `trap_message`
    /// table); the VM's captured-run reports the same message via the
    /// `RunOutcome::Panicked(msg)` outcome (its `PanicInfo::message`, which the
    /// streaming `run_program` path renders identically as `panic: <msg>`). We
    /// compare the native stderr line against the VM's rendered panic line.
    #[test]
    fn diff_trap_message_text_native_eq_vm() {
        use k2_vm::RunOutcome;
        // i32::MIN negation overflows in Debug -> "negation of minimum integer".
        let neg = "pub fn main(sys: *System) !void { _ = sys; \
                   var x: i32 = -2147483648; x = -x; _ = x; }";
        // u64 300 -> u8 narrowing truncates -> "cast truncated value".
        let cast = "pub fn main(sys: *System) !void { _ = sys; \
                    var n: u64 = 300; const t: u8 = @intCast(n); _ = t; }";
        for (src, needle) in [
            (neg, "negation of minimum integer"),
            (cast, "cast truncated value"),
        ] {
            let prog = lower_mode(src, BuildMode::Debug);
            let (n_code, _n_out, n_err) = run_native(&prog);
            let n_err = String::from_utf8_lossy(&n_err).to_string();
            // The VM's panic message, rendered the way `run_program` prints it.
            let (v_outcome, v_code, _v_out, _v_err) =
                k2_vm::run_captured(&prog, k2_vm::RunArgs::new(prog.mode));
            let v_line = match v_outcome {
                RunOutcome::Panicked(msg) => format!("panic: {msg}"),
                other => panic!("`{needle}` expected a VM panic, got {other:?}"),
            };
            assert_eq!(n_code, 134, "`{needle}` traps on native (134)");
            assert_eq!(v_code, 134, "`{needle}` traps on the VM (134)");
            assert!(
                n_err.contains(needle),
                "native stderr must contain `{needle}`, got: {n_err:?}"
            );
            assert_eq!(
                n_err.trim(),
                v_line.trim(),
                "trap text must be byte-identical native==VM for `{needle}`"
            );
        }
    }
}

// =========================================================================
//  Tier 2 — aarch64 ELF validity + cross-compilation (host-independent)
// =========================================================================
//
// These tests compile real k2 MIR through the aarch64 backend and validate the
// emitted EM_AARCH64 ELF *structurally* — they never execute it (there is no
// aarch64 emulator on this host). They run on EVERY host (the bytes are built
// and parsed, not run). This is the honest correctness evidence for the aarch64
// ELF writer + the same-MIR cross-compilation property.
mod aarch64_cross {
    use crate::Target;
    use k2_mir::{lower_program, BuildMode, MirProgram};

    /// Lowers a self-contained k2 source string to a verified `MirProgram`,
    /// mirroring the gated `exec::lower` but available on every host (no
    /// execution). The aarch64 path only *builds* bytes, so this is portable.
    fn lower(source: &str) -> MirProgram {
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
        let mut prog =
            lower_program(&pres.file, &resolved, typed, BuildMode::Debug).expect("lowering failed");
        assert!(prog.is_ok(), "lowering diagnostics in test program");
        k2_opt::optimize(&mut prog, k2_opt::OptLevel::None);
        let problems = prog.verify();
        assert!(problems.is_empty(), "malformed MIR: {problems:?}");
        prog
    }

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

    /// The smallest hello-class program: a bare `write`/`exit` via `print`.
    const HELLO: &str = r#"
        const std = @import("std");
        pub fn main(sys: *System) !void {
            const out = sys.io.stdout();
            try out.print("Hello, k2!\n", .{});
        }
    "#;

    fn read_u16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn read_u32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn read_u64(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn aarch64_elf_header_invariants() {
        let prog = lower(HELLO);
        let img = crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux)
            .expect("aarch64 cross-compile of a hello-class program");
        let b = &img.bytes;
        // ELF magic + class/data/version.
        assert_eq!(&b[0..4], &[0x7f, b'E', b'L', b'F']);
        assert_eq!(b[4], 2, "EI_CLASS == ELFCLASS64");
        assert_eq!(b[5], 1, "EI_DATA == ELFDATA2LSB");
        // e_type == ET_EXEC (2), e_machine == EM_AARCH64 (183).
        assert_eq!(read_u16(b, 16), 2, "e_type == ET_EXEC");
        assert_eq!(read_u16(b, 18), 183, "e_machine == EM_AARCH64");
        // e_entry == 0x401000 (one page in).
        assert_eq!(read_u64(b, 24), 0x40_1000, "e_entry");
        assert_eq!(img.text_vaddr, 0x40_1000);
        // e_phoff/e_ehsize/e_phentsize.
        assert_eq!(read_u64(b, 32), 64);
        assert_eq!(read_u16(b, 52), 64); // e_ehsize
        assert_eq!(read_u16(b, 54), 56); // e_phentsize
        let phnum = read_u16(b, 56);
        assert!(phnum >= 1, "at least one PT_LOAD");
    }

    #[test]
    fn aarch64_elf_has_valid_text_pt_load() {
        let prog = lower(HELLO);
        let img = crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux).unwrap();
        let b = &img.bytes;
        // Phdr 0 is the text segment (PT_LOAD, R+X) mapping the headers + code.
        let p0 = 64;
        assert_eq!(read_u32(b, p0), 1, "p_type == PT_LOAD");
        assert_eq!(read_u32(b, p0 + 4), 5, "p_flags == PF_R|PF_X");
        assert_eq!(read_u64(b, p0 + 8), 0, "p_offset (maps the headers)");
        assert_eq!(read_u64(b, p0 + 16), 0x40_0000, "p_vaddr == load base");
        assert_eq!(read_u64(b, p0 + 48), 0x1000, "p_align == 4 KiB page");
        // The entry instruction (the `_start` shim) is at file offset 0x1000 and is
        // a valid 32-bit aarch64 word (the image is a whole number of words).
        let text_off = 0x1000usize;
        assert!(b.len() > text_off + 4, "image has a text segment");
        assert_eq!(
            img.text_len % 4,
            0,
            "text is a whole number of 32-bit words"
        );
        // The first text word is `mov x0, #0` == movz x0,#0 == 0xD2800000.
        assert_eq!(
            read_u32(b, text_off),
            0xD280_0000,
            "_start begins with movz x0,#0"
        );
    }

    #[test]
    fn aarch64_rodata_segment_congruence() {
        // A program that prints a string literal emits a `.rodata` PT_LOAD; the
        // kernel's mapping congruence `p_vaddr ≡ p_offset (mod p_align)` must hold.
        let prog = lower(HELLO);
        let img = crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux).unwrap();
        let b = &img.bytes;
        let phnum = read_u16(b, 56);
        assert!(phnum >= 2, "a printing program has a rodata segment");
        let p1 = 64 + 56;
        assert_eq!(read_u32(b, p1), 1, "p_type == PT_LOAD");
        assert_eq!(read_u32(b, p1 + 4), 4, "p_flags == PF_R");
        let r_off = read_u64(b, p1 + 8);
        let r_vaddr = read_u64(b, p1 + 16);
        assert_eq!(read_u64(b, p1 + 48), 0x1000);
        assert_eq!(
            r_vaddr % 0x1000,
            r_off % 0x1000,
            "vaddr ≡ offset (mod page)"
        );
        assert_eq!(r_vaddr, img.rodata_vaddr);
    }

    #[test]
    fn same_mir_drives_both_targets() {
        // The decisive property: ONE MIR compiles to BOTH targets. The x86 image is
        // a runnable EM_X86_64 ELF; the aarch64 image is a valid EM_AARCH64 ELF.
        let prog = lower(HELLO);
        let x86 = crate::compile_program_to_elf_for(&prog, Target::X86_64Linux).unwrap();
        let arm = crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux).unwrap();
        assert_eq!(read_u16(&x86.bytes, 18), 0x3E, "x86 image is EM_X86_64");
        assert_eq!(read_u16(&arm.bytes, 18), 183, "aarch64 image is EM_AARCH64");
        // Both are ET_EXEC with a valid entry one page in.
        assert_eq!(read_u64(&x86.bytes, 24), 0x40_1000);
        assert_eq!(read_u64(&arm.bytes, 24), 0x40_1000);
    }

    #[test]
    fn aarch64_defers_heap_runtime_cleanly() {
        // A program needing the *System heap runtime is refused with a clean
        // `Unsupported` deferral on aarch64 (never a miscompile), while x86-64
        // compiles it.
        let src = r#"
            const std = @import("std");
            pub fn main(sys: *System) !void {
                const a = sys.heap;
                const p = try a.create(u64);
                p.* = 7;
                a.destroy(p);
            }
        "#;
        let prog = lower(src);
        // x86 compiles it (the runtime is ported there).
        assert!(crate::compile_program_to_elf_for(&prog, Target::X86_64Linux).is_ok());
        // aarch64 defers cleanly.
        match crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux) {
            Err(crate::CodegenError::Unsupported(msg)) => {
                assert!(
                    msg.contains("runtime") || msg.contains("aarch64"),
                    "deferral message should mention the runtime/aarch64: {msg}"
                );
            }
            Err(other) => panic!("expected a clean Unsupported deferral, got {other:?}"),
            Ok(_) => panic!("aarch64 should defer the heap runtime, not compile it"),
        }
    }

    #[test]
    fn target_triple_parsing() {
        assert_eq!(
            Target::parse_triple("x86_64-linux").unwrap(),
            Target::X86_64Linux
        );
        assert_eq!(
            Target::parse_triple("aarch64-linux").unwrap(),
            Target::Aarch64Linux
        );
        assert_eq!(
            Target::parse_triple("aarch64-unknown-linux-gnu").unwrap(),
            Target::Aarch64Linux
        );
        assert_eq!(
            Target::parse_triple("x86_64-unknown-linux-gnu").unwrap(),
            Target::X86_64Linux
        );
        let err = Target::parse_triple("riscv64-linux").unwrap_err();
        assert!(err.contains("x86_64-linux") && err.contains("aarch64-linux"));
    }

    // =====================================================================
    //  Full-encoder oracle audit (llvm-objdump, gated on availability)
    // =====================================================================
    //
    // The byte-exact UNIT tests above are the always-on, host-portable correctness
    // evidence. This integration test is an ADDITIONAL whole-program cross-check:
    // it disassembles the actually-emitted `hello.aarch64` with `llvm-objdump` (our
    // aarch64 oracle) and asserts the prologue reserves the frame and locals are
    // addressed correctly — the exact properties the v0.18 defects violated
    // (`neg xzr` instead of `sub sp`, `[fp,#0]`-aliased locals). It is NOT a runtime
    // check (aarch64 is not executed here), and `llvm-objdump` is ONLY a dev/test
    // oracle — never a build- or run-time dependency of the compiler. If the tool is
    // absent (some CI hosts), the test skips cleanly so it cannot break the build.

    /// Locates `llvm-objdump` on `PATH`, returning its path or `None` (→ skip).
    fn find_llvm_objdump() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            for name in ["llvm-objdump", "llvm-objdump-21", "llvm-objdump-20"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        None
    }

    #[test]
    fn aarch64_hello_disassembly_oracle_audit() {
        let Some(objdump) = find_llvm_objdump() else {
            eprintln!("skipping: llvm-objdump not found on PATH (oracle unavailable)");
            return;
        };

        // Emit the real hello.aarch64 ELF (frame > 4095, so it hits the large-frame
        // SP-sub path AND has negative local homes — the two fatal v0.18 sites).
        let prog = lower(HELLO);
        let img = crate::compile_program_to_elf_for(&prog, Target::Aarch64Linux).unwrap();
        let dir = std::env::temp_dir();
        let elf = dir.join(format!("k2_oracle_hello_{}.aarch64", std::process::id()));
        std::fs::write(&elf, &img.bytes).expect("write temp elf");

        let out = std::process::Command::new(&objdump)
            .args(["-d", "--triple=aarch64"])
            .arg(&elf)
            .output()
            .expect("run llvm-objdump");
        let _ = std::fs::remove_file(&elf);
        assert!(
            out.status.success(),
            "llvm-objdump failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let disasm = String::from_utf8_lossy(&out.stdout).to_lowercase();

        // (1) The frame is reserved by a REAL `sub sp, ...` (immediate or
        // extended-register), NOT mis-encoded as `neg xzr` / `sub xzr`.
        assert!(
            !disasm.contains("neg\t") && !disasm.contains("neg "),
            "prologue must not contain `neg` (frame never reserved!):\n{disasm}"
        );
        assert!(
            disasm.contains("sub\tsp,") || disasm.contains("sub sp,"),
            "prologue must reserve the frame with `sub sp, ...`:\n{disasm}"
        );
        // No instruction may write the zero register where SP was intended: a
        // `sub xzr,`/`add xzr,` in the frame path is the XZR-vs-SP bug.
        assert!(
            !disasm.contains("sub\txzr,") && !disasm.contains("add\txzr,"),
            "no `sub/add xzr` (XZR-where-SP-intended):\n{disasm}"
        );

        // (2) Locals (negative fp homes) are accessed via the unscaled signed
        // ldur/stur family (or a correct register-offset form), never collapsed to
        // `[x29, #0]`. The old miscompile produced `stur`/`ldur`-less code where
        // every local was `str/ldr x?, [x29]` (no displacement). Require that at
        // least one `stur`/`ldur` appears (the negative homes) and that there is no
        // bare `[x29]` store/load (a zero-displacement fp access is the alias bug).
        assert!(
            disasm.contains("stur") || disasm.contains("ldur"),
            "negative fp-relative locals must use stur/ldur:\n{disasm}"
        );
        // llvm-objdump prints a zero-displacement memory operand as `[x29]` (no
        // `#imm`); any such fp access would be the collapsed-to-[fp,#0] alias.
        for line in disasm.lines() {
            assert!(
                !line.contains("[x29]"),
                "a local collapsed to [x29,#0] (the alias bug): {line}"
            );
        }

        // (3) The exit/syscall sequence is present and correct: an `svc #0`
        // syscall terminates the program (llvm-objdump renders it `svc\t#0`).
        assert!(
            disasm.contains("svc\t#0") || disasm.contains("svc #0"),
            "expected an `svc #0` syscall in the emitted program:\n{disasm}"
        );

        // (4) Spot the specific corrected prologue word: the large frame is
        // reserved via the extended-register sub (`sub sp, sp, x11` family). Confirm
        // the exact bytes 0xCB2B63FF appear in the text (independent of objdump's
        // textual rendering) — the very word the v0.18 backend got wrong.
        let text = &img.bytes[0x1000..0x1000 + img.text_len];
        let has_sub_sp_x11 = text
            .chunks_exact(4)
            .any(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == 0xCB2B_63FF);
        assert!(
            has_sub_sp_x11,
            "emitted text must contain the corrected `sub sp, sp, x11` = 0xCB2B63FF"
        );
        // And the bogus `neg xzr, x11` = 0xCB0B03FF must be GONE.
        let has_neg_xzr = text
            .chunks_exact(4)
            .any(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == 0xCB0B_03FF);
        assert!(
            !has_neg_xzr,
            "the bogus `neg xzr, x11` (0xCB0B03FF) must be gone"
        );
    }
}
