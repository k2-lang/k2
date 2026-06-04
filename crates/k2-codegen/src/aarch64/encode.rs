//! The pure-std AArch64 (ARMv8-A) instruction encoder.
//!
//! AArch64 instructions are **fixed 32 bits, little-endian** — a refreshing
//! contrast to x86-64's variable-length encoding. Every method here appends one
//! `u32` (via [`Aarch64Asm::word`], which writes the four little-endian bytes),
//! computed by OR-ing the instruction's bit-fields into a base opcode. Each field
//! layout is documented in MSB→LSB order, cross-checked against the ARM
//! Architecture Reference Manual (DDI 0487) encoding tables.
//!
//! ## Register model
//!
//! A [`Reg`] is the 5-bit architectural register number: `x0`–`x30` are `0..=30`,
//! and `31` is the stack pointer `sp` *or* the zero register `xzr` depending on
//! the instruction slot (the ARM ARM calls this register `Rn`/`Rd` == 31). We
//! expose [`SP`]/[`XZR`] both as `Reg(31)`; the caller picks the slot.
//!
//! All arithmetic is 64-bit (`sf == 1`), mirroring the x86 backend's
//! always-`REX.W` discipline; the narrowing/width-correctness is handled by
//! explicit `UBFM`/`SBFM` (via [`Aarch64Asm::ubfm`]/[`Aarch64Asm::sbfm`]) and
//! masking exactly where the x86 path uses `movzx`/`movsx`/AND-masks.
//!
//! ## Relocations
//!
//! Branch and address-materialization targets are not known at emit time, so we
//! reuse the SHARED [`Fixup`]/[`FixupKind`] model from [`crate::encode`]: an
//! intra-function branch records a [`FixupKind::Local`]; a cross-function call a
//! [`FixupKind::Call`]; a `.rodata` pointer a [`FixupKind::Data`] (resolved into a
//! 4-instruction `movz/movk×3` immediate — see [`crate::link`]'s
//! `patch_movz_movk`). [`Aarch64Asm::finish`] resolves the local fixups (branch
//! displacements) and returns the surviving cross-function ones, exactly like the
//! x86 [`crate::encode::Asm::finish`].

use crate::encode::{Cc, Fixup, FixupKind, LabelId};
use crate::mir_ids::FnId;

/// An AArch64 architectural register number (`0..=31`). `31` is `sp`/`xzr`
/// depending on the instruction slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Reg(pub u8);

/// The stack pointer (register slot 31 in an `sp`-reading position).
pub const SP: Reg = Reg(31);
/// The zero register (register slot 31 in a `Zr`-reading position).
pub const XZR: Reg = Reg(31);
/// The frame pointer, `x29`, by AAPCS64 convention.
pub const FP: Reg = Reg(29);
/// The link register, `x30`, holding the return address after a `bl`.
pub const LR: Reg = Reg(30);

impl Reg {
    /// The raw 5-bit register number.
    fn n(self) -> u32 {
        self.0 as u32
    }
}

/// The access size of a load/store, selecting the `size` field + sign behavior.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemSize {
    /// 1 byte (`ldrb`/`strb`, `ldrsb`).
    B,
    /// 2 bytes (`ldrh`/`strh`, `ldrsh`).
    H,
    /// 4 bytes (`ldr w`/`str w`, `ldrsw`).
    W,
    /// 8 bytes (`ldr x`/`str x`).
    X,
}

impl MemSize {
    /// The 2-bit `size` field (00/01/10/11).
    fn size_field(self) -> u32 {
        match self {
            MemSize::B => 0b00,
            MemSize::H => 0b01,
            MemSize::W => 0b10,
            MemSize::X => 0b11,
        }
    }
    /// The byte width, used to scale an unsigned-offset immediate (`imm12` is in
    /// units of the access size).
    fn bytes(self) -> i64 {
        match self {
            MemSize::B => 1,
            MemSize::H => 2,
            MemSize::W => 4,
            MemSize::X => 8,
        }
    }
}

/// The AArch64 instruction assembler: an append-only buffer of 32-bit words plus
/// the SHARED fixup/label machinery. Its public method surface mirrors the x86
/// [`crate::encode::Asm`] where the lowering needs it (`bind_label`,
/// `new_local_label`, label-relative branches, cross-function calls), so the
/// link pass can stitch both targets uniformly.
#[derive(Default)]
pub struct Aarch64Asm {
    /// The raw machine-code bytes (always a whole number of 4-byte words).
    buf: Vec<u8>,
    /// Cross-function / data fixups that survive `finish`.
    fixups: Vec<Fixup>,
    /// `label_offsets[id] = byte offset the label marks`, or `None` until bound.
    label_offsets: Vec<Option<usize>>,
    /// Pending intra-function branch fixups: `(byte offset of the instruction,
    /// label, branch kind)`, resolved by [`Aarch64Asm::finish`].
    local_branches: Vec<(usize, LabelId, BranchKind)>,
}

/// The displacement width of an intra-function branch, selecting how `finish`
/// patches its immediate field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BranchKind {
    /// `b`/`bl` — a 26-bit signed word displacement (`imm26`).
    Imm26,
    /// `b.cc`/`cbz`/`cbnz` — a 19-bit signed word displacement (`imm19`).
    Imm19,
}

// This is a deliberately *complete* encoder for the instruction set the blueprint
// specifies (§2.1): every method emits one verified instruction and is byte-exact
// unit-tested in `aarch64/tests.rs`. A subset of them — `movn`, `mov_imm_state`,
// `adrp`/`add` address pairs, `madd`, `smulh`/`umulh`, `lsr_imm`, `adds`, `tst`,
// and the `fadd`/`fsub`/… scalar-double family — are part of that complete,
// tested surface but are not reached by the *hello-class* aarch64 lowering (which
// uses `movz/movk` absolute addressing, the `msub`-based remainder, and no FP /
// state segment yet). The `dead_code` allow documents that intended completeness
// — exactly as the x86 `encode.rs` does for its analogous untaken-but-tested
// methods — rather than masking an accidental dead path; the unit tests prove
// every one of these encodes correctly.
#[allow(dead_code)]
impl Aarch64Asm {
    /// A fresh, empty assembler.
    pub fn new() -> Aarch64Asm {
        Aarch64Asm::default()
    }

    /// The current byte position (always a multiple of 4).
    pub fn pos(&self) -> usize {
        self.buf.len()
    }

    /// The raw bytes emitted so far (a test-only peek).
    #[cfg(test)]
    pub fn code(&self) -> &[u8] {
        &self.buf
    }

    /// Appends one 32-bit instruction word, little-endian.
    fn word(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    // =====================================================================
    //  Moves & immediates
    // =====================================================================

    /// `movz Rd, #imm16, LSL #(hw*16)` — zero-out then set one 16-bit lane.
    /// Encoding: `1 10 100101 hw(2) imm16(16) Rd(5)` (base `0xD2800000`).
    pub fn movz(&mut self, rd: Reg, imm16: u16, hw: u8) {
        let w = 0xD280_0000 | ((hw as u32 & 0b11) << 21) | ((imm16 as u32) << 5) | rd.n();
        self.word(w);
    }

    /// `movk Rd, #imm16, LSL #(hw*16)` — keep the other lanes, set one lane.
    /// Encoding: `1 11 100101 hw imm16 Rd` (base `0xF2800000`).
    pub fn movk(&mut self, rd: Reg, imm16: u16, hw: u8) {
        let w = 0xF280_0000 | ((hw as u32 & 0b11) << 21) | ((imm16 as u32) << 5) | rd.n();
        self.word(w);
    }

    /// `movn Rd, #imm16, LSL #(hw*16)` — move the bitwise-NOT of one lane (used to
    /// materialize small negative constants compactly).
    /// Encoding: `1 00 100101 hw imm16 Rd` (base `0x92800000`).
    pub fn movn(&mut self, rd: Reg, imm16: u16, hw: u8) {
        let w = 0x9280_0000 | ((hw as u32 & 0b11) << 21) | ((imm16 as u32) << 5) | rd.n();
        self.word(w);
    }

    /// Materializes an arbitrary 64-bit immediate into `rd` using the minimal
    /// `movz`+`movk` sequence: `movz` the lowest non-trivial lane, then `movk`
    /// every remaining nonzero lane. A zero materializes as a single `movz #0`.
    /// This is the AArch64 analog of x86's `mov r64, imm64`.
    pub fn mov_imm(&mut self, rd: Reg, imm: i64) {
        let u = imm as u64;
        let lanes = [
            (u & 0xFFFF) as u16,
            ((u >> 16) & 0xFFFF) as u16,
            ((u >> 32) & 0xFFFF) as u16,
            ((u >> 48) & 0xFFFF) as u16,
        ];
        // Find the first lane to seed with movz. If all lanes are zero, movz #0.
        let first = lanes.iter().position(|&l| l != 0);
        match first {
            None => self.movz(rd, 0, 0),
            Some(i0) => {
                self.movz(rd, lanes[i0], i0 as u8);
                for (i, &l) in lanes.iter().enumerate().skip(i0 + 1) {
                    if l != 0 {
                        self.movk(rd, l, i as u8);
                    }
                }
            }
        }
    }

    /// `mov Rd, Rn` (register copy) = `orr Rd, xzr, Rn`.
    pub fn mov_reg(&mut self, rd: Reg, rn: Reg) {
        self.orr(rd, XZR, rn);
    }

    /// `mov Rd, sp` / `mov sp, Rn` — the SP-form move (`add Rd, Rn, #0`), distinct
    /// from the `orr`-based register move because `xzr`/`sp` share slot 31.
    pub fn mov_sp(&mut self, rd: Reg, rn: Reg) {
        self.add_imm(rd, rn, 0);
    }

    /// Materializes a `.rodata` pointer hole: a 4-instruction `movz`+`movk×3`
    /// sequence (`mov_imm`-shaped) whose 16-bit lanes are patched at link time to
    /// the string's absolute virtual address. Records a [`FixupKind::Data`] at the
    /// `movz`. The four words are ALWAYS emitted (even for zero lanes) so the patch
    /// site has a fixed, known shape.
    pub fn mov_imm_data(&mut self, rd: Reg, data_off: u32) {
        let at = self.pos();
        self.movz(rd, 0, 0);
        self.movk(rd, 0, 1);
        self.movk(rd, 0, 2);
        self.movk(rd, 0, 3);
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::Data(data_off),
        });
    }

    /// Materializes a writable state-segment pointer hole, like
    /// [`Aarch64Asm::mov_imm_data`] but resolving to `state_vaddr + offset`.
    pub fn mov_imm_state(&mut self, rd: Reg, state_off: u32) {
        let at = self.pos();
        self.movz(rd, 0, 0);
        self.movk(rd, 0, 1);
        self.movk(rd, 0, 2);
        self.movk(rd, 0, 3);
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::State(state_off),
        });
    }

    /// `adrp Rd, page` — form a PC-relative ±4 GiB page address.
    /// Encoding: `1 immlo(2) 10000 immhi(19) Rd`. This encoder is provided + tested
    /// for completeness (the lowering uses the `movz/movk` absolute form instead,
    /// matching the x86 relocation model); the `imm` here is the raw 21-bit page
    /// delta, already in units of 4 KiB.
    pub fn adrp(&mut self, rd: Reg, imm21: i32) {
        let imm = (imm21 as u32) & 0x1F_FFFF;
        let immlo = imm & 0b11;
        let immhi = (imm >> 2) & 0x7_FFFF;
        let w = 0x9000_0000 | (immlo << 29) | (immhi << 5) | rd.n();
        self.word(w);
    }

    // =====================================================================
    //  ALU — shifted register
    // =====================================================================

    /// `add Rd, Rn, Rm` (64-bit, no shift).
    /// Encoding: `sf 0 0 01011 shift(2)=00 0 Rm imm6=0 Rn Rd` (base `0x8B000000`).
    pub fn add(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x8B00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `sub Rd, Rn, Rm` (base `0xCB000000`).
    pub fn sub(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xCB00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `add Rd, Rn, Rm` in the **extended-register** form (`option = UXTX`,
    /// `imm3 = 0`) — i.e. `add Rd, Rn, Rm, uxtx #0`, which is value-identical to
    /// the plain `add Rd, Rn, Rm` EXCEPT that register slot 31 in `Rd`/`Rn` means
    /// the **stack pointer `sp`**, not the zero register `xzr`. The shifted-register
    /// [`Aarch64Asm::add`] reads slot 31 as `xzr`, so it CANNOT name `sp`; this form
    /// is the only register-`add` that can target/base `sp`.
    ///
    /// Encoding: `sf 0 0 01011 00 1 Rm option(3) imm3(3) Rn Rd`
    /// (base `0x8B200000`; `option = 0b011 = UXTX` in bits 13..15, `imm3 = 0`).
    /// Verified with llvm-mc: `add x2, sp, x15` == `0x8B2F63E2`.
    pub fn add_ext(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x8B20_0000 | (rm.n() << 16) | (0b011 << 13) | (rn.n() << 5) | rd.n());
    }

    /// `sub Rd, Rn, Rm` in the **extended-register** form (`option = UXTX`,
    /// `imm3 = 0`) — the SP-capable subtract (slot 31 means `sp`, not `xzr`). This
    /// is what the prologue's large-frame reservation needs: `sub sp, sp, x11`.
    ///
    /// Encoding: `sf 1 0 01011 00 1 Rm option(3) imm3(3) Rn Rd`
    /// (base `0xCB200000`; `option = 0b011 = UXTX`, `imm3 = 0`).
    /// Verified with llvm-mc: `sub sp, sp, x11` == `0xCB2B63FF`.
    pub fn sub_ext(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xCB20_0000 | (rm.n() << 16) | (0b011 << 13) | (rn.n() << 5) | rd.n());
    }

    /// `add Rd, Rn, Rm, LSL #sh` — shifted add, used to scale an index by a
    /// power-of-two element stride in one instruction. `sh` is 0..=63.
    pub fn add_lsl(&mut self, rd: Reg, rn: Reg, rm: Reg, sh: u8) {
        self.word(
            0x8B00_0000 | (rm.n() << 16) | ((sh as u32 & 0x3F) << 10) | (rn.n() << 5) | rd.n(),
        );
    }

    /// `and Rd, Rn, Rm` (base `0x8A000000`).
    pub fn and(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x8A00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `orr Rd, Rn, Rm` (base `0xAA000000`).
    pub fn orr(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xAA00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `eor Rd, Rn, Rm` (base `0xCA000000`).
    pub fn eor(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xCA00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `orn Rd, Rn, Rm` — `Rn | ~Rm`; with `Rn == xzr` this is `mvn`/`not`.
    /// Encoding: `sf 01 01010 shift 1 Rm imm6 Rn Rd` (base `0xAA200000`).
    pub fn orn(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xAA20_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `mvn Rd, Rm` = `orn Rd, xzr, Rm` — bitwise NOT.
    pub fn mvn(&mut self, rd: Reg, rm: Reg) {
        self.orn(rd, XZR, rm);
    }

    /// `neg Rd, Rm` = `sub Rd, xzr, Rm` — two's-complement negate.
    pub fn neg(&mut self, rd: Reg, rm: Reg) {
        self.sub(rd, XZR, rm);
    }

    /// `mul Rd, Rn, Rm` = `madd Rd, Rn, Rm, xzr`.
    /// Encoding: `sf 00 11011 000 Rm 0 Ra=11111 Rn Rd` (base `0x9B007C00`).
    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9B00_7C00 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `madd Rd, Rn, Rm, Ra` — `Ra + Rn*Rm`.
    /// Encoding: `sf 00 11011 000 Rm 0 Ra Rn Rd` (base `0x9B000000`).
    pub fn madd(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        self.word(0x9B00_0000 | (rm.n() << 16) | (ra.n() << 10) | (rn.n() << 5) | rd.n());
    }

    /// `msub Rd, Rn, Rm, Ra` — `Ra - Rn*Rm`; the remainder building block
    /// (`rem = a - (a/b)*b`).
    /// Encoding: `sf 00 11011 000 Rm 1 Ra Rn Rd` (base `0x9B008000`).
    pub fn msub(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        self.word(0x9B00_8000 | (rm.n() << 16) | (ra.n() << 10) | (rn.n() << 5) | rd.n());
    }

    /// `sdiv Rd, Rn, Rm` — signed divide.
    /// Encoding: `sf 0 0 11010110 Rm 000011 Rn Rd` (base `0x9AC00C00`).
    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9AC0_0C00 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `udiv Rd, Rn, Rm` — unsigned divide.
    /// Encoding: `sf 0 0 11010110 Rm 000010 Rn Rd` (base `0x9AC00800`).
    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9AC0_0800 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `smulh Rd, Rn, Rm` — signed high 64 bits of a 128-bit product (signed
    /// multiply-overflow check). Encoding base `0x9B407C00`.
    pub fn smulh(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9B40_7C00 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `umulh Rd, Rn, Rm` — unsigned high 64 bits of the product. Base `0x9BC07C00`.
    pub fn umulh(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9BC0_7C00 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    // =====================================================================
    //  ALU — register shifts
    // =====================================================================

    /// `lslv Rd, Rn, Rm` — logical shift left by a register amount.
    /// Encoding: `sf 0 0 11010110 Rm 0010 00 Rn Rd` (base `0x9AC02000`).
    pub fn lslv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9AC0_2000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `lsrv Rd, Rn, Rm` — logical shift right (base `0x9AC02400`).
    pub fn lsrv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9AC0_2400 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `asrv Rd, Rn, Rm` — arithmetic (sign-propagating) shift right
    /// (base `0x9AC02800`).
    pub fn asrv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0x9AC0_2800 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `ubfm Rd, Rn, #immr, #imms` — the bitfield-move primitive behind immediate
    /// `lsl`/`lsr` and unsigned narrows. Encoding (64-bit, `N=1`):
    /// `1 10 100110 1 immr(6) imms(6) Rn Rd` (base `0xD3400000`).
    pub fn ubfm(&mut self, rd: Reg, rn: Reg, immr: u8, imms: u8) {
        self.word(
            0xD340_0000
                | ((immr as u32 & 0x3F) << 16)
                | ((imms as u32 & 0x3F) << 10)
                | (rn.n() << 5)
                | rd.n(),
        );
    }

    /// `sbfm Rd, Rn, #immr, #imms` — signed bitfield move (sign-extending narrows /
    /// `asr` immediate). Base `0x93400000` (64-bit, `N=1`).
    pub fn sbfm(&mut self, rd: Reg, rn: Reg, immr: u8, imms: u8) {
        self.word(
            0x9340_0000
                | ((immr as u32 & 0x3F) << 16)
                | ((imms as u32 & 0x3F) << 10)
                | (rn.n() << 5)
                | rd.n(),
        );
    }

    /// `lsl Rd, Rn, #sh` — immediate logical shift left, via `ubfm`.
    pub fn lsl_imm(&mut self, rd: Reg, rn: Reg, sh: u8) {
        let sh = sh & 0x3F;
        self.ubfm(rd, rn, (64 - sh) & 0x3F, 63 - sh);
    }

    /// `lsr Rd, Rn, #sh` — immediate logical shift right, via `ubfm`.
    pub fn lsr_imm(&mut self, rd: Reg, rn: Reg, sh: u8) {
        self.ubfm(rd, rn, sh & 0x3F, 63);
    }

    /// `asr Rd, Rn, #sh` — immediate arithmetic shift right, via `sbfm`.
    pub fn asr_imm(&mut self, rd: Reg, rn: Reg, sh: u8) {
        self.sbfm(rd, rn, sh & 0x3F, 63);
    }

    // =====================================================================
    //  ALU — immediate
    // =====================================================================

    /// `add Rd, Rn, #imm12` (unsigned 12-bit immediate, no shift).
    /// Encoding: `sf 0 0 100010 sh=0 imm12 Rn Rd` (base `0x91000000`).
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, imm12: u32) {
        self.word(0x9100_0000 | ((imm12 & 0xFFF) << 10) | (rn.n() << 5) | rd.n());
    }

    /// `sub Rd, Rn, #imm12` (base `0xD1000000`).
    pub fn sub_imm(&mut self, rd: Reg, rn: Reg, imm12: u32) {
        self.word(0xD100_0000 | ((imm12 & 0xFFF) << 10) | (rn.n() << 5) | rd.n());
    }

    /// `add Rd, Rn, #imm` for a non-negative immediate that may exceed the 12-bit
    /// field. A ≤12-bit immediate is one `add`; a larger one is materialized into a
    /// scratch (`x15`) and added as a register. This keeps frame/struct-offset
    /// addressing total without a panic.
    pub fn add_imm_pos(&mut self, rd: Reg, rn: Reg, imm: u32) {
        if imm <= 0xFFF {
            self.add_imm(rd, rn, imm);
        } else if imm & 0xFFF == 0 && (imm >> 12) <= 0xFFF {
            // add with the LSL #12 shift form: `sh=1`.
            self.word(0x9140_0000 | (((imm >> 12) & 0xFFF) << 10) | (rn.n() << 5) | rd.n());
        } else {
            self.mov_imm(Reg(15), imm as i64);
            // Use the extended-register `add` (UXTX #0) rather than the
            // shifted-register one: it is value-identical for general registers but,
            // crucially, names `sp` when `rn == 31` (the shifted form would read slot
            // 31 as `xzr` and silently drop the SP base — see [`Aarch64Asm::add_ext`]).
            self.add_ext(rd, rn, Reg(15));
        }
    }

    /// `add Rd, Rn, #disp` where `disp` may be **negative** (a frame-home offset is
    /// `fp - k`). A negative displacement becomes a `sub`; large magnitudes are
    /// materialized into `x15`. This is the address-of-a-stack-home primitive.
    pub fn add_imm_to(&mut self, rd: Reg, rn: Reg, disp: i32) {
        if disp >= 0 {
            self.add_imm_pos(rd, rn, disp as u32);
        } else {
            let mag = (-(disp as i64)) as u32;
            if mag <= 0xFFF {
                self.sub_imm(rd, rn, mag);
            } else {
                self.mov_imm(Reg(15), disp as i64);
                // SP-capable add (see the `add_imm_pos` fallthrough comment): `rn`
                // may be `sp` here (address-of a deep stack home), so the
                // extended-register form is required.
                self.add_ext(rd, rn, Reg(15));
            }
        }
    }

    /// `adds Rd, Rn, #imm12` — flag-setting add (base `0xB1000000`).
    pub fn adds_imm(&mut self, rd: Reg, rn: Reg, imm12: u32) {
        self.word(0xB100_0000 | ((imm12 & 0xFFF) << 10) | (rn.n() << 5) | rd.n());
    }

    /// `subs Rd, Rn, #imm12` — flag-setting subtract (base `0xF1000000`).
    /// `subs xzr, Rn, #imm12` is `cmp Rn, #imm12`.
    pub fn subs_imm(&mut self, rd: Reg, rn: Reg, imm12: u32) {
        self.word(0xF100_0000 | ((imm12 & 0xFFF) << 10) | (rn.n() << 5) | rd.n());
    }

    /// `cmp Rn, #imm12` = `subs xzr, Rn, #imm12`.
    pub fn cmp_imm(&mut self, rn: Reg, imm12: u32) {
        self.subs_imm(XZR, rn, imm12);
    }

    // =====================================================================
    //  ALU — flag-setting register (compares & overflow checks)
    // =====================================================================

    /// `adds Rd, Rn, Rm` — flag-setting add (base `0xAB000000`).
    pub fn adds(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xAB00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `subs Rd, Rn, Rm` — flag-setting subtract (base `0xEB000000`).
    pub fn subs(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.word(0xEB00_0000 | (rm.n() << 16) | (rn.n() << 5) | rd.n());
    }

    /// `cmp Rn, Rm` = `subs xzr, Rn, Rm`.
    pub fn cmp(&mut self, rn: Reg, rm: Reg) {
        self.subs(XZR, rn, rm);
    }

    /// `tst Rn, Rm` = `ands xzr, Rn, Rm` — flag-setting AND.
    /// Encoding: `sf 11 01010 shift 0 Rm imm6 Rn Rd=11111` (base `0xEA00001F`).
    pub fn tst(&mut self, rn: Reg, rm: Reg) {
        self.word(0xEA00_0000 | (rm.n() << 16) | (rn.n() << 5) | 0x1F);
    }

    /// `cset Rd, cc` = `csinc Rd, xzr, xzr, invert(cc)` — materialize a condition
    /// into a 0/1 boolean. Encoding: `sf 0 0 11010100 Rm=11111 cond_inv(4) 0 1
    /// Rn=11111 Rd` (base `0x9A9F07E0`, with the inverted condition in bits 12..15).
    pub fn cset(&mut self, rd: Reg, cc: Cc) {
        // csinc uses the *inverted* condition (low bit flipped).
        let cond_inv = (cc.cond4() ^ 1) as u32;
        self.word(0x9A9F_07E0 | (cond_inv << 12) | rd.n());
    }

    // =====================================================================
    //  Loads & stores
    // =====================================================================
    //
    // Three addressing forms are emitted, chosen by `off` (see `store`/`load`):
    //
    //   * **unsigned scaled** `ldr/str [Rn, #imm12]` — `off` a non-negative
    //     multiple of the access size, `off/size <= 0xFFF`. This is the densest
    //     form but CANNOT name a negative offset (the field is unsigned), which is
    //     exactly why every fp-relative *local* (homed at `fp - k`, i.e. NEGATIVE)
    //     needs one of the next two forms.
    //   * **unscaled signed** `ldur/stur [Rn, #imm9]` — `off` in `-256..=255`
    //     (any alignment). This covers the common negative local homes.
    //   * **register offset** `ldr/str [Rn, Xm]` — for `off` outside both ranges
    //     (a deep stack home): the byte displacement is materialized into the
    //     encoder's address scratch `x17` and added (UXTX, so a NEGATIVE `off`'s
    //     64-bit two's complement subtracts correctly).
    //
    // The fatal v0.18 bug this replaces: the old code used ONLY the unsigned scaled
    // form and silently clamped any negative `off` to `imm12 == 0`, so every local
    // aliased `[fp, #0]` (the saved {fp,lr} slot) — a total miscompile.

    /// The encoder's dedicated address-materialization scratch for the
    /// register-offset load/store form. `x17` (IP1) is intra-procedure-call
    /// scratch and is NOT used by the aarch64 lowering for any live value, so
    /// clobbering it here cannot corrupt a lowering temporary (unlike `x15`, which
    /// the digit-emitter holds live across stores).
    const MEM_SCRATCH: Reg = Reg(17);

    /// `str Rt, [Rn, #off]` — store the full register (or a sub-width slice per
    /// `size`). `off` is a signed byte offset and may be negative; the addressing
    /// form is selected as documented on the section above. Encoding (scaled):
    /// `size(2) 111 0 01 00 imm12 Rn Rt` (opc=00 for store).
    pub fn store(&mut self, rt: Reg, rn: Reg, off: i64, size: MemSize) {
        if let Some(imm12) = scaled_off(off, size) {
            let w =
                (size.size_field() << 30) | 0x3900_0000 | (imm12 << 10) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else if let Some(imm9) = unscaled_off(off) {
            // stur: `size 111 0 00 00 0 imm9 00 Rn Rt` (opc=00).
            let w = (size.size_field() << 30) | 0x3800_0000 | (imm9 << 12) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else {
            self.mov_imm(Self::MEM_SCRATCH, off);
            self.store_reg_off(rt, rn, Self::MEM_SCRATCH, size);
        }
    }

    /// `ldr Rt, [Rn, #off]` — load, zero-extending sub-width sizes. Same addressing
    /// selection as [`Aarch64Asm::store`]; scaled base `0x39400000` (opc=01).
    pub fn load(&mut self, rt: Reg, rn: Reg, off: i64, size: MemSize) {
        if let Some(imm12) = scaled_off(off, size) {
            let w =
                (size.size_field() << 30) | 0x3940_0000 | (imm12 << 10) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else if let Some(imm9) = unscaled_off(off) {
            // ldur: opc=01 (`0x38400000` family).
            let w = (size.size_field() << 30) | 0x3840_0000 | (imm9 << 12) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else {
            self.mov_imm(Self::MEM_SCRATCH, off);
            self.load_reg_off(rt, rn, Self::MEM_SCRATCH, false, size);
        }
    }

    /// `ldrs{b,h,w} Rt, [Rn, #off]` — sign-extending sub-width load (64-bit
    /// destination, `opc=10`). `MemSize::X` falls back to a plain `ldr` (a 64-bit
    /// load has nothing to sign-extend). Same addressing selection as `load`; the
    /// unscaled form is `ldurs{b,h,w}` (`0x38800000` family).
    pub fn load_signed(&mut self, rt: Reg, rn: Reg, off: i64, size: MemSize) {
        if size == MemSize::X {
            return self.load(rt, rn, off, size);
        }
        if let Some(imm12) = scaled_off(off, size) {
            let w =
                (size.size_field() << 30) | 0x3980_0000 | (imm12 << 10) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else if let Some(imm9) = unscaled_off(off) {
            // ldurs{b,h}: opc=10 (`0x38800000` family).
            let w = (size.size_field() << 30) | 0x3880_0000 | (imm9 << 12) | (rn.n() << 5) | rt.n();
            self.word(w);
        } else {
            self.mov_imm(Self::MEM_SCRATCH, off);
            self.load_reg_off(rt, rn, Self::MEM_SCRATCH, true, size);
        }
    }

    /// `str Rt, [Rn, Rm]` — register-offset store (option=UXTX, S=0, i.e. `Rm`
    /// added with no scaling). Encoding:
    /// `size 111 0 00 00 1 Rm option(3)=011 S=0 10 Rn Rt` (base `0x38206800`).
    /// Verified with llvm-mc: `str x0, [x1, x15]` == `0xF82F6820`.
    fn store_reg_off(&mut self, rt: Reg, rn: Reg, rm: Reg, size: MemSize) {
        let w = (size.size_field() << 30) | 0x3820_6800 | (rm.n() << 16) | (rn.n() << 5) | rt.n();
        self.word(w);
    }

    /// `ldr Rt, [Rn, Rm]` — register-offset load (UXTX). `signed` selects the
    /// sign-extending `ldrs{b,h,w}` form (opc=10, base `0x38A06800`) over the
    /// zero-extending `ldr` (opc=01, base `0x38606800`). A 64-bit `signed` load has
    /// nothing to extend and uses the zero-extending base.
    fn load_reg_off(&mut self, rt: Reg, rn: Reg, rm: Reg, signed: bool, size: MemSize) {
        let base = if signed && size != MemSize::X {
            0x38A0_6800
        } else {
            0x3860_6800
        };
        let w = (size.size_field() << 30) | base | (rm.n() << 16) | (rn.n() << 5) | rt.n();
        self.word(w);
    }

    /// `stp Rt, Rt2, [sp, #-16]!` — pre-indexed store-pair, the AAPCS64 frame save
    /// of `{x29, x30}`. Encoding (64-bit pre-index):
    /// `10 101 0 011 0 imm7 Rt2 Rn Rt` (base `0xA9800000`); `imm7 = -16/8 = -2`
    /// (7-bit signed = `0x7E`), `Rn = sp = 31`.
    pub fn stp_pre(&mut self, rt: Reg, rt2: Reg, rn: Reg, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        let w = 0xA980_0000 | (imm7 << 15) | (rt2.n() << 10) | (rn.n() << 5) | rt.n();
        self.word(w);
    }

    /// `ldp Rt, Rt2, [sp], #16` — post-indexed load-pair, the frame restore of
    /// `{x29, x30}`. Encoding (64-bit post-index):
    /// `10 101 0 001 1 imm7 Rt2 Rn Rt` (base `0xA8C00000`); `imm7 = 16/8 = 2`.
    pub fn ldp_post(&mut self, rt: Reg, rt2: Reg, rn: Reg, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        let w = 0xA8C0_0000 | (imm7 << 15) | (rt2.n() << 10) | (rn.n() << 5) | rt.n();
        self.word(w);
    }

    // =====================================================================
    //  Control flow
    // =====================================================================

    /// `ret` (`ret x30`) — return to the link register. Encoding `0xD65F03C0`.
    pub fn ret(&mut self) {
        self.word(0xD65F_03C0);
    }

    /// `svc #0` — supervisor call (a Linux syscall). Encoding `0xD4000001`.
    pub fn svc0(&mut self) {
        self.word(0xD400_0001);
    }

    /// `b label` — unconditional branch to a function-local label (`imm26`). The
    /// displacement is patched by [`Aarch64Asm::finish`].
    pub fn b(&mut self, label: LabelId) {
        let at = self.pos();
        self.local_branches.push((at, label, BranchKind::Imm26));
        self.word(0x1400_0000); // b #0
    }

    /// `b.cc label` — conditional branch (`imm19`).
    pub fn bcc(&mut self, cc: Cc, label: LabelId) {
        let at = self.pos();
        self.local_branches.push((at, label, BranchKind::Imm19));
        self.word(0x5400_0000 | cc.cond4() as u32); // b.cc #0
    }

    /// `bl func` — branch-with-link to another function (an `imm26` call). Records a
    /// [`FixupKind::Call`] for the program layout pass.
    pub fn bl_fn(&mut self, func: FnId) {
        let at = self.pos();
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::Call(func),
        });
        self.word(0x9400_0000); // bl #0
    }

    // =====================================================================
    //  FP (scalar double) — provided + tested for the capability surface
    // =====================================================================

    /// `fadd Dd, Dn, Dm` (base `0x1E602800`).
    pub fn fadd(&mut self, dd: Reg, dn: Reg, dm: Reg) {
        self.word(0x1E60_2800 | (dm.n() << 16) | (dn.n() << 5) | dd.n());
    }
    /// `fsub Dd, Dn, Dm` (base `0x1E603800`).
    pub fn fsub(&mut self, dd: Reg, dn: Reg, dm: Reg) {
        self.word(0x1E60_3800 | (dm.n() << 16) | (dn.n() << 5) | dd.n());
    }
    /// `fmul Dd, Dn, Dm` (base `0x1E600800`).
    pub fn fmul(&mut self, dd: Reg, dn: Reg, dm: Reg) {
        self.word(0x1E60_0800 | (dm.n() << 16) | (dn.n() << 5) | dd.n());
    }
    /// `fdiv Dd, Dn, Dm` (base `0x1E601800`).
    pub fn fdiv(&mut self, dd: Reg, dn: Reg, dm: Reg) {
        self.word(0x1E60_1800 | (dm.n() << 16) | (dn.n() << 5) | dd.n());
    }
    /// `fcmp Dn, Dm` (base `0x1E602000`).
    pub fn fcmp(&mut self, dn: Reg, dm: Reg) {
        self.word(0x1E60_2000 | (dm.n() << 16) | (dn.n() << 5));
    }
    /// `scvtf Dd, Xn` — signed 64-bit int → double (base `0x9E620000`).
    pub fn scvtf(&mut self, dd: Reg, xn: Reg) {
        self.word(0x9E62_0000 | (xn.n() << 5) | dd.n());
    }
    /// `fcvtzs Xd, Dn` — double → signed 64-bit int, truncating (base `0x9E780000`).
    pub fn fcvtzs(&mut self, xd: Reg, dn: Reg) {
        self.word(0x9E78_0000 | (dn.n() << 5) | xd.n());
    }

    // =====================================================================
    //  Labels & finalization
    // =====================================================================

    /// Reserves `n` distinct labels (`LabelId(0)..LabelId(n)`).
    pub fn reserve_labels(&mut self, n: usize) {
        self.label_offsets = vec![None; n];
    }

    /// Binds `label` at the current position.
    pub fn bind_label(&mut self, label: LabelId) {
        self.label_offsets[label.0 as usize] = Some(self.pos());
    }

    /// Allocates a fresh dynamically-created local label (render-loop targets).
    pub fn new_local_label(&mut self) -> LabelId {
        let id = LabelId(self.label_offsets.len() as u32);
        self.label_offsets.push(None);
        id
    }

    /// Binds a dynamically-allocated local label at the current position.
    pub fn bind_local(&mut self, label: LabelId) {
        self.label_offsets[label.0 as usize] = Some(self.pos());
    }

    /// An unconditional branch to a local label.
    pub fn b_local(&mut self, label: LabelId) {
        self.b(label);
    }

    /// A conditional branch to a local label.
    pub fn bcc_local(&mut self, cc: Cc, label: LabelId) {
        self.bcc(cc, label);
    }

    /// Resolves all intra-function branch fixups against the bound label offsets,
    /// patching each `imm26`/`imm19` field, and returns the finalized bytes plus
    /// the surviving cross-function/data fixups.
    pub fn finish(mut self) -> (Vec<u8>, Vec<Fixup>) {
        for (at, label, kind) in &self.local_branches {
            let target = self.label_offsets[label.0 as usize]
                .expect("aarch64: unbound local label at finish");
            // Word displacement from the branch instruction to the target.
            let rel_bytes = target as i64 - *at as i64;
            let rel_words = rel_bytes / 4;
            let base = u32::from_le_bytes([
                self.buf[*at],
                self.buf[*at + 1],
                self.buf[*at + 2],
                self.buf[*at + 3],
            ]);
            let patched = match kind {
                BranchKind::Imm26 => {
                    let imm26 = (rel_words as u32) & 0x03FF_FFFF;
                    (base & 0xFC00_0000) | imm26
                }
                BranchKind::Imm19 => {
                    let imm19 = (rel_words as u32) & 0x0007_FFFF;
                    (base & 0xFF00_001F) | (imm19 << 5)
                }
            };
            self.buf[*at..*at + 4].copy_from_slice(&patched.to_le_bytes());
        }
        (self.buf, self.fixups)
    }
}

/// Tries to scale a byte offset into the unsigned-offset `imm12` field. Returns
/// `Some(imm12)` only when `off` is a NON-NEGATIVE multiple of the access size and
/// the scaled value fits 12 bits; otherwise `None` (the caller falls back to the
/// unscaled `ldur`/`stur` or register-offset form). It NEVER silently clamps a
/// negative offset to zero — that was the v0.18 miscompile.
fn scaled_off(off: i64, size: MemSize) -> Option<u32> {
    let bytes = size.bytes();
    if off < 0 || off % bytes != 0 {
        return None;
    }
    let scaled = off / bytes;
    if scaled > 0xFFF {
        return None;
    }
    Some(scaled as u32)
}

/// Tries to encode a byte offset into the unscaled signed 9-bit `imm9` field of the
/// `ldur`/`stur` family (range `-256..=255`, any alignment). Returns the raw 9-bit
/// two's-complement field ready to OR into bits 12..20, or `None` if out of range.
fn unscaled_off(off: i64) -> Option<u32> {
    if (-256..=255).contains(&off) {
        Some((off as u32) & 0x1FF)
    } else {
        None
    }
}
