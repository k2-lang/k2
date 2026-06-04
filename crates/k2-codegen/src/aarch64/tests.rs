//! Byte-exact unit tests for the AArch64 encoder.
//!
//! AArch64 cannot be executed or disassembled in this environment (no
//! `qemu-aarch64`, no aarch64 binutils, and the host `objdump` knows only the
//! i386/x86-64 BFD targets). So these tests are the PRIMARY, honest correctness
//! evidence for the encoder: each asserts the emitted 4-byte little-endian word
//! against the value the ARM Architecture Reference Manual (DDI 0487) assigns to
//! that exact instruction. They build bytes only — nothing runs — so they pass on
//! every host. (They are NOT validated against a running CPU.)

use super::encode::{Aarch64Asm, MemSize, Reg, FP, LR, SP, XZR};
use crate::encode::Cc;

/// Builds a single instruction via `f` and returns its little-endian bytes.
fn enc(f: impl FnOnce(&mut Aarch64Asm)) -> [u8; 4] {
    let mut a = Aarch64Asm::new();
    f(&mut a);
    let c = a.code();
    assert_eq!(c.len(), 4, "expected exactly one 32-bit instruction");
    [c[0], c[1], c[2], c[3]]
}

/// Asserts the encoded word equals `expect` (given as the natural big-endian
/// `u32`); the bytes in the stream are its little-endian serialization.
fn assert_word(f: impl FnOnce(&mut Aarch64Asm), expect: u32) {
    let got = u32::from_le_bytes(enc(f));
    assert_eq!(
        got, expect,
        "encoding mismatch: got {got:#010X}, expected {expect:#010X}"
    );
}

const X0: Reg = Reg(0);
const X1: Reg = Reg(1);
const X2: Reg = Reg(2);

#[test]
fn movz_movk_movn() {
    assert_word(|a| a.movz(X0, 0, 0), 0xD280_0000); // movz x0, #0
    assert_word(|a| a.movz(X0, 0x1234, 0), 0xD282_4680); // movz x0, #0x1234
    assert_word(|a| a.movk(X0, 0xFFFF, 1), 0xF2BF_FFE0); // movk x0, #0xFFFF, lsl 16
    assert_word(|a| a.movn(X0, 0, 0), 0x9280_0000); // movn x0, #0
}

#[test]
fn mov_imm_builds_64bit() {
    // movz x0, #0 for zero.
    let mut a = Aarch64Asm::new();
    a.mov_imm(X0, 0);
    assert_eq!(a.code(), &0xD280_0000u32.to_le_bytes());

    // A value touching all four lanes is movz + 3 movk.
    let mut a = Aarch64Asm::new();
    a.mov_imm(X0, 0x1111_2222_3333_4444u64 as i64);
    assert_eq!(a.code().len(), 16);
    let w: Vec<u32> = a
        .code()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(w[0] & 0xFFE0_0000, 0xD280_0000); // movz, hw=0
    assert_eq!(w[1] & 0xFFE0_0000, 0xF2A0_0000); // movk, hw=1
    assert_eq!(w[2] & 0xFFE0_0000, 0xF2C0_0000); // movk, hw=2
    assert_eq!(w[3] & 0xFFE0_0000, 0xF2E0_0000); // movk, hw=3
}

#[test]
fn alu_shifted_register() {
    assert_word(|a| a.add(X0, X0, X1), 0x8B01_0000); // add  x0, x0, x1
    assert_word(|a| a.sub(X0, X0, X1), 0xCB01_0000); // sub  x0, x0, x1
    assert_word(|a| a.and(X0, X0, X1), 0x8A01_0000); // and  x0, x0, x1
    assert_word(|a| a.orr(X0, X0, X1), 0xAA01_0000); // orr  x0, x0, x1
    assert_word(|a| a.eor(X0, X0, X1), 0xCA01_0000); // eor  x0, x0, x1
    assert_word(|a| a.mul(X0, X0, X1), 0x9B01_7C00); // mul  x0, x0, x1
    assert_word(|a| a.sdiv(X0, X0, X1), 0x9AC1_0C00); // sdiv x0, x0, x1
    assert_word(|a| a.udiv(X0, X0, X1), 0x9AC1_0800); // udiv x0, x0, x1
    assert_word(|a| a.msub(X0, X2, X1, X0), 0x9B01_8040); // msub x0, x2, x1, x0
    assert_word(|a| a.madd(X0, X0, X1, XZR), 0x9B01_7C00); // madd x0,x0,x1,xzr == mul
}

#[test]
fn neg_and_not() {
    assert_word(|a| a.neg(X0, X1), 0xCB01_03E0); // neg x0, x1  (sub x0, xzr, x1)
    assert_word(|a| a.mvn(X0, X1), 0xAA21_03E0); // mvn x0, x1  (orn x0, xzr, x1)
}

#[test]
fn shifts() {
    assert_word(|a| a.lslv(X0, X0, X1), 0x9AC1_2000); // lsl x0, x0, x1
    assert_word(|a| a.lsrv(X0, X0, X1), 0x9AC1_2400); // lsr x0, x0, x1
    assert_word(|a| a.asrv(X0, X0, X1), 0x9AC1_2800); // asr x0, x0, x1
    assert_word(|a| a.lsl_imm(X0, X0, 1), 0xD37F_F800); // lsl x0, x0, #1
    assert_word(|a| a.lsr_imm(X0, X0, 1), 0xD341_FC00); // lsr x0, x0, #1
    assert_word(|a| a.asr_imm(X0, X0, 1), 0x9341_FC00); // asr x0, x0, #1
}

#[test]
fn alu_immediate() {
    assert_word(|a| a.add_imm(X0, X0, 16), 0x9100_4000); // add x0, x0, #16
    assert_word(|a| a.sub_imm(X0, X0, 16), 0xD100_4000); // sub x0, x0, #16
    assert_word(|a| a.add_imm(SP, SP, 32), 0x9100_83FF); // add sp, sp, #32
    assert_word(|a| a.sub_imm(SP, SP, 32), 0xD100_83FF); // sub sp, sp, #32
    assert_word(|a| a.cmp_imm(X0, 0), 0xF100_001F); // cmp x0, #0
    assert_word(|a| a.cmp_imm(X0, 5), 0xF100_141F); // cmp x0, #5
}

#[test]
fn compares_and_cset() {
    assert_word(|a| a.cmp(X0, X1), 0xEB01_001F); // cmp x0, x1 (subs xzr,x0,x1)
    assert_word(|a| a.subs(X0, X0, X1), 0xEB01_0000); // subs x0, x0, x1
    assert_word(|a| a.adds(X0, X0, X1), 0xAB01_0000); // adds x0, x0, x1
    assert_word(|a| a.tst(X0, X0), 0xEA00_001F); // tst x0, x0
    assert_word(|a| a.cset(X0, Cc::E), 0x9A9F_17E0); // cset x0, eq
    assert_word(|a| a.cset(X0, Cc::Ne), 0x9A9F_07E0); // cset x0, ne
    assert_word(|a| a.cset(X0, Cc::L), 0x9A9F_A7E0); // cset x0, lt
}

#[test]
fn smulh_umulh() {
    assert_word(|a| a.smulh(X0, X0, X1), 0x9B41_7C00); // smulh x0, x0, x1
    assert_word(|a| a.umulh(X0, X0, X1), 0x9BC1_7C00); // umulh x0, x0, x1
}

#[test]
fn loads_and_stores_all_sizes() {
    // str/ldr x (64-bit), offset 0 and [fp,#off].
    assert_word(|a| a.store(X0, X0, 0, MemSize::X), 0xF900_0000); // str x0, [x0]
    assert_word(|a| a.load(X0, X0, 0, MemSize::X), 0xF940_0000); // ldr x0, [x0]
    assert_word(|a| a.load(X0, FP, 8, MemSize::X), 0xF940_07A0); // ldr x0, [x29, #8]
    assert_word(|a| a.store(X0, FP, 8, MemSize::X), 0xF900_07A0); // str x0, [x29, #8]
                                                                  // Byte / half / word.
    assert_word(|a| a.store(X0, X1, 0, MemSize::B), 0x3900_0020); // strb w0, [x1]
    assert_word(|a| a.load(X0, X1, 0, MemSize::B), 0x3940_0020); // ldrb w0, [x1]
    assert_word(|a| a.store(X0, X1, 0, MemSize::H), 0x7900_0020); // strh w0, [x1]
    assert_word(|a| a.load(X0, X1, 0, MemSize::H), 0x7940_0020); // ldrh w0, [x1]
    assert_word(|a| a.store(X0, X1, 0, MemSize::W), 0xB900_0020); // str  w0, [x1]
    assert_word(|a| a.load(X0, X1, 0, MemSize::W), 0xB940_0020); // ldr  w0, [x1]
                                                                 // Signed sub-width loads.
    assert_word(|a| a.load_signed(X0, X1, 0, MemSize::B), 0x3980_0020); // ldrsb x0, [x1]
    assert_word(|a| a.load_signed(X0, X1, 0, MemSize::H), 0x7980_0020); // ldrsh x0, [x1]
    assert_word(|a| a.load_signed(X0, X1, 0, MemSize::W), 0xB980_0020); // ldrsw x0, [x1]
                                                                        // Scaled offset: ldr x0, [x1, #16]  ->  imm12 = 16/8 = 2.
    assert_word(|a| a.load(X0, X1, 16, MemSize::X), 0xF940_0820);
}

#[test]
fn sp_extended_register_add_sub() {
    // The SP-capable register add/sub: slot 31 names `sp`, NOT `xzr`. These are the
    // forms the prologue's large-frame reservation and deep stack-home addressing
    // need. All four words are llvm-mc verified (LLVM 21):
    //   `sub sp, sp, x11`  == [0xff,0x63,0x2b,0xcb] == 0xCB2B63FF
    //   `add x2, sp, x15`  == [0xe2,0x63,0x2f,0x8b] == 0x8B2F63E2
    //   `sub x0, sp, x11`  == [0xe0,0x63,0x2b,0xcb] == 0xCB2B63E0
    //   `add sp, sp, x15`  == [0xff,0x63,0x2f,0x8b] == 0x8B2F63FF
    assert_word(|a| a.sub_ext(SP, SP, Reg(11)), 0xCB2B_63FF); // sub sp, sp, x11
    assert_word(|a| a.add_ext(X2, SP, Reg(15)), 0x8B2F_63E2); // add x2, sp, x15
    assert_word(|a| a.sub_ext(X0, SP, Reg(11)), 0xCB2B_63E0); // sub x0, sp, x11
    assert_word(|a| a.add_ext(SP, SP, Reg(15)), 0x8B2F_63FF); // add sp, sp, x15
}

#[test]
fn add_imm_pos_large_sp_offset_uses_extended_add() {
    // A >0xFFF non-4096-multiple SP offset materializes into x15 then uses the
    // EXTENDED add (so `sp` is the base, not `xzr`). The second word must be
    // `add x2, sp, x15` == 0x8B2F63E2, NOT the shifted-register 0x8B0F0042.
    let mut a = Aarch64Asm::new();
    a.add_imm_pos(X2, SP, 0x1234);
    let words: Vec<u32> = a
        .code()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let last = *words.last().expect("at least one word");
    assert_eq!(last, 0x8B2F_63E2, "large SP offset must use extended add");
}

#[test]
fn stur_ldur_negative_offsets_all_widths() {
    // Every fp-relative local is homed at a NEGATIVE offset; the unscaled signed
    // ldur/stur family is the only immediate form that can name it. All words are
    // llvm-mc verified (LLVM 21):
    //   `stur x0, [x29, #-8]`   == 0xF81F83A0      `ldur x0, [x29, #-8]`   == 0xF85F83A0
    //   `stur x0, [x29, #-256]` == 0xF81003A0      `ldur x0, [x29, #255]`  == 0xF84FF3A0
    //   `stur x0, [x29, #0]`    == 0xF80003A0      `ldur x0, [x29, #0]`    == 0xF84003A0
    assert_word(|a| a.store(X0, FP, -8, MemSize::X), 0xF81F_83A0); // stur x0, [x29,#-8]
    assert_word(|a| a.load(X0, FP, -8, MemSize::X), 0xF85F_83A0); // ldur x0, [x29,#-8]
    assert_word(|a| a.store(X0, FP, -256, MemSize::X), 0xF810_03A0); // stur, min imm9
    assert_word(|a| a.load(X0, FP, 255, MemSize::X), 0xF84F_F3A0); // ldur, max imm9 (unaligned)
                                                                   // Zero offset stays in the scaled form (imm12=0) but the words match the
                                                                   // unscaled #0 word too is NOT what we want; assert the scaled form here.
    assert_word(|a| a.store(X0, FP, 0, MemSize::X), 0xF900_03A0); // str x0, [x29] (scaled)
    assert_word(|a| a.load(X0, FP, 0, MemSize::X), 0xF940_03A0); // ldr x0, [x29] (scaled)

    // Byte / half / word negative stores and loads (unscaled), llvm-mc verified:
    //   `sturb w0,[x1,#-1]`=0x381FF020  `ldurb w0,[x1,#-1]`=0x385FF020
    //   `sturh w0,[x1,#-2]`=0x781FE020  `ldurh w0,[x1,#-2]`=0x785FE020
    //   `stur  w0,[x1,#-4]`=0xB81FC020  `ldur  w0,[x1,#-4]`=0xB85FC020
    assert_word(|a| a.store(X0, X1, -1, MemSize::B), 0x381F_F020); // sturb w0, [x1,#-1]
    assert_word(|a| a.load(X0, X1, -1, MemSize::B), 0x385F_F020); // ldurb w0, [x1,#-1]
    assert_word(|a| a.store(X0, X1, -2, MemSize::H), 0x781F_E020); // sturh w0, [x1,#-2]
    assert_word(|a| a.load(X0, X1, -2, MemSize::H), 0x785F_E020); // ldurh w0, [x1,#-2]
    assert_word(|a| a.store(X0, X1, -4, MemSize::W), 0xB81F_C020); // stur  w0, [x1,#-4]
    assert_word(|a| a.load(X0, X1, -4, MemSize::W), 0xB85F_C020); // ldur  w0, [x1,#-4]

    // Sign-extending unscaled loads (ldurs{b,h,w}), llvm-mc verified:
    //   `ldursb x0,[x1,#-1]`=0x389FF020  `ldursh x0,[x1,#-2]`=0x789FE020
    //   `ldursw x0,[x1,#-4]`=0xB89FC020
    assert_word(|a| a.load_signed(X0, X1, -1, MemSize::B), 0x389F_F020); // ldursb x0,[x1,#-1]
    assert_word(|a| a.load_signed(X0, X1, -2, MemSize::H), 0x789F_E020); // ldursh x0,[x1,#-2]
    assert_word(|a| a.load_signed(X0, X1, -4, MemSize::W), 0xB89F_C020); // ldursw x0,[x1,#-4]
}

#[test]
fn deep_negative_offset_uses_register_offset_form() {
    // A negative offset beyond the imm9 range (-256..255) cannot use ldur/stur, so
    // it materializes the displacement into x17 (movz/movk) and uses the UXTX
    // register-offset form. The FINAL word must be the register-offset store/load,
    // llvm-mc verified:
    //   `str x0, [x29, x17]` == 0xF8316BA0      `ldr x0, [x29, x17]` == 0xF8716BA0
    let mut a = Aarch64Asm::new();
    a.store(X0, FP, -8400, MemSize::X);
    let words: Vec<u32> = a
        .code()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert!(words.len() >= 2, "materialize + register-offset store");
    assert_eq!(
        *words.last().unwrap(),
        0xF831_6BA0,
        "deep negative store must be `str x0, [x29, x17]`"
    );
    // None of the words may be the bogus `[fp,#0]` scaled store (0xF90003A0) that
    // the old silent-clamp produced.
    assert!(
        !words.contains(&0xF900_03A0),
        "must not collapse a deep local to [fp,#0]"
    );

    let mut a = Aarch64Asm::new();
    a.load(X0, FP, -8400, MemSize::X);
    let last = u32::from_le_bytes({
        let c = a.code();
        let n = c.len();
        [c[n - 4], c[n - 3], c[n - 2], c[n - 1]]
    });
    assert_eq!(
        last, 0xF871_6BA0,
        "deep negative load must be `ldr x0, [x29, x17]`"
    );
}

#[test]
fn distinct_negative_homes_do_not_alias() {
    // Regression for the v0.18 silent-clamp: two distinct negative homes must encode
    // to DIFFERENT words (the old code collapsed every negative offset to [fp,#0]).
    let w8 = u32::from_le_bytes(enc(|a| a.store(X0, FP, -8, MemSize::X)));
    let w16 = u32::from_le_bytes(enc(|a| a.store(X0, FP, -16, MemSize::X)));
    let w24 = u32::from_le_bytes(enc(|a| a.store(X0, FP, -24, MemSize::X)));
    assert_ne!(w8, w16);
    assert_ne!(w16, w24);
    assert_ne!(w8, w24);
    // And none is the saved-{fp,lr} slot store `str x0,[x29,#0]` (0xF90003A0).
    for w in [w8, w16, w24] {
        assert_ne!(w, 0xF900_03A0, "a local must not alias [fp,#0]");
    }
}

#[test]
fn frame_save_pair() {
    // stp x29, x30, [sp, #-16]!
    assert_word(|a| a.stp_pre(FP, LR, SP, -16), 0xA9BF_7BFD);
    // ldp x29, x30, [sp], #16
    assert_word(|a| a.ldp_post(FP, LR, SP, 16), 0xA8C1_7BFD);
}

#[test]
fn control_flow() {
    assert_word(|a| a.ret(), 0xD65F_03C0); // ret
    assert_word(|a| a.svc0(), 0xD400_0001); // svc #0
                                            // b/b.cc/bl to self (displacement 0): only the opcode + condition remain.
    assert_word(
        |a| {
            a.reserve_labels(1);
            a.bind_label(crate::encode::LabelId(0));
            a.b(crate::encode::LabelId(0));
        },
        0x1400_0000,
    );
    assert_word(
        |a| {
            a.reserve_labels(1);
            a.bind_label(crate::encode::LabelId(0));
            a.bcc(Cc::E, crate::encode::LabelId(0));
        },
        0x5400_0000,
    );
}

#[test]
fn branch_displacements_resolve() {
    // forward b: bind a label two words ahead, branch should encode imm26 = +2.
    let mut a = Aarch64Asm::new();
    a.reserve_labels(1);
    let l = crate::encode::LabelId(0);
    a.b(l); // word 0
    a.movz(X0, 0, 0); // word 1
    a.bind_label(l); // word 2
    let (bytes, _) = a.finish();
    let w0 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(w0, 0x1400_0002, "b should branch forward two words");

    // backward b.eq: the label is bound at offset 0, the branch is the second
    // word (byte 4), so the displacement is -1 word -> imm19 = -1.
    let mut a = Aarch64Asm::new();
    a.reserve_labels(1);
    let l = crate::encode::LabelId(0);
    a.bind_label(l); // offset 0
    a.movz(X0, 0, 0); // word 0 (bytes 0..4)
    a.bcc(Cc::E, l); // word 1 (bytes 4..8), branches back one word
    let (bytes, _) = a.finish();
    let w1 = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    // imm19 = -1 (0x7FFFF) in bits 5..23; cond eq = 0.
    assert_eq!(w1, 0x5400_0000 | ((0x7FFFFu32 & 0x7FFFF) << 5));
}

#[test]
fn adrp_add_address_pair() {
    // adrp x0, +0 ; add x0, x0, #0  -- the PC-relative address pair (provided for
    // completeness; the lowering uses movz/movk absolute addressing instead).
    assert_word(|a| a.adrp(X0, 0), 0x9000_0000);
    assert_word(|a| a.add_imm(X0, X0, 0), 0x9100_0000);
}

#[test]
fn fp_scalar_double() {
    let d0 = Reg(0);
    let d1 = Reg(1);
    assert_word(|a| a.fadd(d0, d0, d1), 0x1E61_2800); // fadd d0, d0, d1
    assert_word(|a| a.fsub(d0, d0, d1), 0x1E61_3800); // fsub d0, d0, d1
    assert_word(|a| a.fmul(d0, d0, d1), 0x1E61_0800); // fmul d0, d0, d1
    assert_word(|a| a.fdiv(d0, d0, d1), 0x1E61_1800); // fdiv d0, d0, d1
    assert_word(|a| a.fcmp(d0, d1), 0x1E61_2000); // fcmp d0, d1
    assert_word(|a| a.scvtf(d0, X0), 0x9E62_0000); // scvtf d0, x0
    assert_word(|a| a.fcvtzs(X0, d0), 0x9E78_0000); // fcvtzs x0, d0
}

#[test]
fn data_pointer_emits_four_word_hole_with_fixup() {
    let mut a = Aarch64Asm::new();
    a.mov_imm_data(X0, 0x40);
    let (bytes, fixups) = a.finish();
    assert_eq!(bytes.len(), 16, "movz + 3 movk = 4 words");
    assert_eq!(fixups.len(), 1);
    assert_eq!(fixups[0].at, 0);
    assert!(matches!(
        fixups[0].kind,
        crate::encode::FixupKind::Data(0x40)
    ));
}
