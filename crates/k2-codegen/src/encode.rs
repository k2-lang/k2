//! The pure-std x86-64 instruction encoder.
//!
//! This module turns one logical machine instruction into the exact raw bytes
//! the CPU executes. There is no assembler text and no external crate — every
//! method appends the REX/opcode/ModRM/SIB/displacement/immediate bytes by hand,
//! following the only three encoding rules the v0.14 subset needs:
//!
//! * **REX prefix** `0100 WRXB` (`0x40` base): `W=1` (giving `0x48`) selects a
//!   64-bit operand size; `R` extends the ModRM `reg` field, `X` extends the SIB
//!   index (never used here — no scaled indexing), and `B` extends the ModRM
//!   `rm` field / the `+r` opcode register / the SIB base. Every operation we
//!   emit is 64-bit, so the prefix is always present.
//! * **ModRM** `mm reg rm`: `mod` (2 bits) selects the addressing form, `reg`
//!   (3 bits, +REX.R) is a register operand or an opcode extension (`/digit`),
//!   and `rm` (3 bits, +REX.B) is the second register or the memory base.
//!   `mod=11` is register-direct; `mod=01`/`10` with `rm=101` is
//!   `[rbp + disp8]` / `[rbp + disp32]` — our only memory form. We never use
//!   `mod=00` with `rm=101` (that means RIP-relative), so a stack slot at
//!   displacement 0 still encodes a `disp8` of 0.
//! * **Immediates / displacements** are little-endian, sign-extended to the
//!   operand width by the CPU where the opcode says so (e.g. `C7 /0 id`).
//!
//! Branch and call targets are not known when the instruction is emitted, so
//! each control-flow method writes a 4-byte placeholder and records a [`Fixup`].
//! Intra-function label fixups are resolved by [`Asm::finish`]; cross-function
//! `call` and `.rodata` references survive as [`Fixup`]s for the program layout
//! pass (`layout.rs`) to patch once every function's code offset and the rodata
//! base address are known.
//!
//! Every method is independently unit-testable: build a fresh [`Asm`], call one
//! method, and assert `asm.code()` equals a known-good byte vector captured from
//! the system assembler (see `tests.rs`).

use crate::reg::Gpr;

/// A condition code, naming the `setcc`/`jcc` variant to emit. The encoding adds
/// the [`Cc::tttn`] nibble to a base opcode: `0F 80+tttn` for a near `jcc`,
/// `0F 90+tttn` for a `setcc`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cc {
    /// Equal / zero (`ZF=1`). `tttn = 4`.
    E,
    /// Not equal / not zero (`ZF=0`). `tttn = 5`.
    Ne,
    /// Signed less-than (`SF != OF`). `tttn = 0xC`.
    L,
    /// Signed less-or-equal (`ZF=1 or SF != OF`). `tttn = 0xE`.
    Le,
    /// Signed greater-than (`ZF=0 and SF == OF`). `tttn = 0xF`.
    G,
    /// Signed greater-or-equal (`SF == OF`). `tttn = 0xD`.
    Ge,
    /// Unsigned below (`CF=1`). `tttn = 2`.
    B,
    /// Unsigned below-or-equal (`CF=1 or ZF=1`). `tttn = 6`.
    Be,
    /// Unsigned above (`CF=0 and ZF=0`). `tttn = 7`.
    A,
    /// Unsigned above-or-equal (`CF=0`). `tttn = 3`.
    Ae,
    /// Overflow (`OF=1`). `tttn = 0`. Drives the signed add/sub/mul overflow
    /// check (`seto`) — the 64-bit overflow predicate reads the CPU's OF flag.
    O,
    /// Carry (`CF=1`). `tttn = 2` (same encoding as [`Cc::B`]). Drives the
    /// unsigned add/sub overflow check (`setc`) — an unsigned add/sub overflows
    /// exactly when it carries/borrows out of the top bit.
    C,
}

impl Cc {
    /// The 4-bit condition encoding (`tttn`) added to the `setcc`/`jcc` opcode.
    pub fn tttn(self) -> u8 {
        match self {
            Cc::E => 0x4,
            Cc::Ne => 0x5,
            Cc::L => 0xC,
            Cc::Le => 0xE,
            Cc::G => 0xF,
            Cc::Ge => 0xD,
            Cc::B => 0x2,
            Cc::Be => 0x6,
            Cc::A => 0x7,
            Cc::Ae => 0x3,
            Cc::O => 0x0,
            Cc::C => 0x2,
        }
    }
}

/// An identifier for a label local to one function. Labels mark basic-block
/// boundaries; an intra-function `jmp`/`jcc` references one and is patched by
/// [`Asm::finish`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LabelId(pub u32);

/// What a [`Fixup`]'s 4-byte (or 8-byte) hole resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FixupKind {
    /// A `rel32` displacement to a function-local label, resolved by
    /// [`Asm::finish`] as `target_offset - (hole_offset + 4)`.
    Local(LabelId),
    /// A `rel32` displacement to another function's entry (an `E8` call). The
    /// program layout pass patches it once every function's code offset is known.
    Call(crate::mir_ids::FnId),
    /// An 8-byte absolute pointer into `.rodata` at the given byte offset. The
    /// layout pass patches it to `rodata_vaddr + offset` (a `mov r64, imm64`
    /// loads the string's absolute virtual address — the ELF is non-PIE, so the
    /// address is fixed at link time).
    Data(u32),
}

/// One unresolved reference embedded in the code stream.
#[derive(Clone, Copy, Debug)]
pub struct Fixup {
    /// The byte offset of the placeholder hole within this function's code.
    pub at: usize,
    /// What the hole resolves to.
    pub kind: FixupKind,
}

/// The instruction assembler: an append-only machine-code buffer plus the list
/// of unresolved [`Fixup`]s and the recorded byte offset of each bound label.
#[derive(Default)]
pub struct Asm {
    /// The raw machine-code bytes accumulated so far.
    buf: Vec<u8>,
    /// Every emitted branch/call/data reference still needing a target.
    fixups: Vec<Fixup>,
    /// `label_offsets[id] = byte offset of the instruction the label marks`, or
    /// `None` until the label is bound by [`Asm::bind_label`].
    label_offsets: Vec<Option<usize>>,
}

// This is a deliberately *complete* encoder for the instruction set the
// blueprint specifies (§1.3): every method emits one verified instruction and is
// unit-tested down to the byte. A few of them — `code` (a test-only byte peek),
// `lea_rbp`, `pop`, `add_rsp_imm`, and the zero-extending `movzx8`/`movzx16` —
// are part of that complete surface but not reached by v0.14's *lowering*, which
// uses `leave` for the epilogue, sign-extension for signed narrows, and AND
// masks for unsigned ones, and takes no addresses. The `dead_code` allow
// documents that intended completeness (the methods are validated by the encoder
// tests) rather than masking an accidental dead path.
#[allow(dead_code)]
impl Asm {
    /// A fresh, empty assembler.
    pub fn new() -> Asm {
        Asm::default()
    }

    /// The current code length, i.e. the byte offset the next instruction lands
    /// at. Used to mark block/label positions and to compute fixup distances.
    pub fn pos(&self) -> usize {
        self.buf.len()
    }

    /// A read-only view of the bytes emitted so far (the unit tests assert
    /// against this).
    pub fn code(&self) -> &[u8] {
        &self.buf
    }

    /// Appends one raw byte.
    fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    /// Appends a little-endian `i32` (used for `disp32`, `imm32`, and `rel32`).
    fn imm32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends a little-endian `i64` (used for `mov r64, imm64`).
    fn imm64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    // ---------------------------------------------------------------------
    //  REX / ModRM helpers
    // ---------------------------------------------------------------------

    /// Emits a REX prefix `0100 WRXB`. We always emit it for v0.14 because every
    /// operation is 64-bit (`W=1`); the `r`/`x`/`b` flags carry the high bits of
    /// the extended registers.
    fn rex(&mut self, w: bool, r: bool, x: bool, b: bool) {
        let mut byte = 0x40u8;
        if w {
            byte |= 0b1000;
        }
        if r {
            byte |= 0b0100;
        }
        if x {
            byte |= 0b0010;
        }
        if b {
            byte |= 0b0001;
        }
        self.byte(byte);
    }

    /// Emits a register-direct ModRM byte (`mod=11`), with `reg` in the
    /// reg field and `rm` in the rm field.
    fn modrm_rr(&mut self, reg: Gpr, rm: Gpr) {
        self.byte(0b11_000_000 | (reg.low3() << 3) | rm.low3());
    }

    /// Emits a `[rbp + disp]` memory ModRM (+ displacement bytes), with the
    /// 3-bit `reg_field` (a register's low 3 bits, or a `/digit` opcode
    /// extension) in the reg slot and `rbp` (rm=101) as the base. Picks the
    /// `disp8` form (`mod=01`) when the displacement fits a signed byte, else
    /// `disp32` (`mod=10`). We never use `mod=00` for `rbp` (that is the
    /// RIP-relative escape), so displacement 0 still emits a one-byte `0x00`.
    fn modrm_rbp_disp(&mut self, reg_field: u8, disp: i32) {
        const RBP_RM: u8 = 0b101;
        if (-128..=127).contains(&disp) {
            self.byte(0b01_000_000 | ((reg_field & 0b111) << 3) | RBP_RM);
            self.byte(disp as i8 as u8);
        } else {
            self.byte(0b10_000_000 | ((reg_field & 0b111) << 3) | RBP_RM);
            self.imm32(disp);
        }
    }

    /// Emits a `[base + disp]` memory ModRM (+ SIB/displacement bytes) for an
    /// arbitrary base register, with the 3-bit `reg_field` (a register's low 3
    /// bits, or a `/digit` opcode extension) in the reg slot.
    ///
    /// Three architectural special cases are handled:
    ///
    /// * **`rsp`/`r12` as base** (`rm=100`): the `rm=100` encoding means "a SIB
    ///   byte follows", so a SIB with `index=100` (no index) and `base=100`/`100`
    ///   is appended to actually address `[rsp]`/`[r12]`.
    /// * **`rbp`/`r13` with disp 0** (`rm=101`): `mod=00 rm=101` is the
    ///   RIP-relative escape, so a zero displacement must still use the `disp8`
    ///   form (`mod=01`, one `0x00` byte) — exactly as `modrm_rbp_disp` does.
    /// * Every other base uses the plain `disp8`/`disp32` form.
    ///
    /// The caller must already have set REX.B from `base.is_ext()` and REX.R from
    /// the reg operand. `base.low3()` selects the rm/SIB-base field.
    fn modrm_mem(&mut self, reg_field: u8, base: Gpr, disp: i32) {
        let rm = base.low3();
        let needs_sib = rm == 0b100; // rsp / r12
                                     // Always emit a displacement (disp8 when it fits a signed byte, else
                                     // disp32), so we never select the `mod=00 rm=101` RIP-relative escape for
                                     // an rbp/r13 base — a zero displacement still encodes a one-byte disp8.
        let use_disp8 = (-128..=127).contains(&disp);
        let mod_bits: u8 = if use_disp8 { 0b01 } else { 0b10 };
        self.byte((mod_bits << 6) | ((reg_field & 0b111) << 3) | rm);
        if needs_sib {
            // SIB: scale=00, index=100 (none), base=rm.
            self.byte((0b100 << 3) | rm);
        }
        if use_disp8 {
            self.byte(disp as i8 as u8);
        } else {
            self.imm32(disp);
        }
    }

    // ---------------------------------------------------------------------
    //  MOV family
    // ---------------------------------------------------------------------

    /// `mov dst, src` (register to register): `REX.W 89 /r`, with `src` in the
    /// reg field and `dst` in the rm field (the `MOV r/m64, r64` direction).
    pub fn mov_rr(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, src.is_ext(), false, dst.is_ext());
        self.byte(0x89);
        self.modrm_rr(src, dst);
    }

    /// `mov dst32, src32` (32-bit operand, **no** `REX.W`): `89 /r`. Writing a
    /// 32-bit GPR auto-zero-extends bits 32–63 of the full 64-bit register, so a
    /// `mov eax, eax` masks RAX to its low 32 bits — the correct, non-sign-
    /// extending way to normalize an unsigned `u32` value to its width. (A
    /// `REX.W 81 /4 0xFFFFFFFF` AND-mask would instead sign-extend the immediate
    /// to all-ones and be a no-op; see `normalize`.) A `REX.B`/`REX.R` prefix is
    /// still emitted for `r8`–`r15`, but never `REX.W`.
    pub fn mov_rr32(&mut self, dst: Gpr, src: Gpr) {
        if src.is_ext() || dst.is_ext() {
            self.rex(false, src.is_ext(), false, dst.is_ext());
        }
        self.byte(0x89);
        self.modrm_rr(src, dst);
    }

    /// `mov dst, imm`: loads a 64-bit immediate. When the value fits a signed
    /// 32-bit range we use the compact sign-extending `REX.W C7 /0 id` form
    /// (7 bytes); otherwise the full `REX.W B8+rd io` form (10 bytes).
    pub fn mov_ri(&mut self, dst: Gpr, imm: i64) {
        if let Ok(imm32) = i32::try_from(imm) {
            // REX.W C7 /0 id — MOV r/m64, imm32 (sign-extended).
            self.rex(true, false, false, dst.is_ext());
            self.byte(0xC7);
            self.modrm_rr_op(0, dst);
            self.imm32(imm32);
        } else {
            // REX.W B8+rd io — MOV r64, imm64.
            self.rex(true, false, false, dst.is_ext());
            self.byte(0xB8 | dst.low3());
            self.imm64(imm);
        }
    }

    /// Emits a register-direct ModRM whose reg field is a `/digit` opcode
    /// extension rather than a register.
    fn modrm_rr_op(&mut self, op_ext: u8, rm: Gpr) {
        self.byte(0b11_000_000 | ((op_ext & 0b111) << 3) | rm.low3());
    }

    /// `mov dst, [rbp + disp]` (load a stack slot): `REX.W 8B /r`.
    pub fn mov_load(&mut self, dst: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x8B);
        self.modrm_rbp_disp(dst.low3(), disp);
    }

    /// `mov [rbp + disp], src` (store a stack slot): `REX.W 89 /r`.
    pub fn mov_store(&mut self, disp: i32, src: Gpr) {
        self.rex(true, src.is_ext(), false, false);
        self.byte(0x89);
        self.modrm_rbp_disp(src.low3(), disp);
    }

    /// `lea dst, [rbp + disp]` (address of a stack slot): `REX.W 8D /r`.
    pub fn lea_rbp(&mut self, dst: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x8D);
        self.modrm_rbp_disp(dst.low3(), disp);
    }

    /// `lea dst, [base + disp]` (address of an arbitrary memory operand):
    /// `REX.W 8D /r`. Used to compute the effective address of a projected place
    /// (the base register already holds an interior pointer).
    pub fn lea_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x8D);
        self.modrm_mem(dst.low3(), base, disp);
    }

    // ---------------------------------------------------------------------
    //  Sized loads / stores through an arbitrary base register
    // ---------------------------------------------------------------------

    /// `mov dst, [base + disp]` (64-bit load): `REX.W 8B /r`.
    pub fn mov_load_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x8B);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov [base + disp], src` (64-bit store): `REX.W 89 /r`.
    pub fn mov_store_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        self.rex(true, src.is_ext(), false, base.is_ext());
        self.byte(0x89);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `movzx dst, byte [base + disp]`: `REX.W 0F B6 /r` (zero-extend a byte).
    pub fn movzx8_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xB6);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsx dst, byte [base + disp]`: `REX.W 0F BE /r` (sign-extend a byte).
    pub fn movsx8_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xBE);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movzx dst, word [base + disp]`: `REX.W 0F B7 /r` (zero-extend a word).
    pub fn movzx16_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xB7);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsx dst, word [base + disp]`: `REX.W 0F BF /r` (sign-extend a word).
    pub fn movsx16_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xBF);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov dst32, [base + disp]` (32-bit load, **no** `REX.W`): `8B /r`. The CPU
    /// zero-extends bits 32–63, the correct way to load an unsigned 32-bit field.
    pub fn mov_load32_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        if dst.is_ext() || base.is_ext() {
            self.rex(false, dst.is_ext(), false, base.is_ext());
        }
        self.byte(0x8B);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsxd dst, [base + disp]` (32-bit sign-extending load): `REX.W 63 /r`.
    pub fn movsxd_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x63);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov byte [base + disp], src8`: `88 /r` — stores the low byte of `src`.
    /// A REX prefix is emitted whenever the base is extended OR `src` is one of
    /// `rsp`/`rbp`/`rsi`/`rdi`/`r8`–`r15`, because without REX the `88 /r` reg
    /// field `4..7` would name `ah/ch/dh/bh` rather than `spl/bpl/sil/dil`.
    pub fn mov_store8_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        if src.is_ext() || base.is_ext() || src.num() >= 4 {
            self.rex(false, src.is_ext(), false, base.is_ext());
        }
        self.byte(0x88);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `mov word [base + disp], src16`: `66 89 /r` (the `66` operand-size prefix
    /// selects a 16-bit store of the low word of `src`).
    pub fn mov_store16_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        self.byte(0x66);
        if src.is_ext() || base.is_ext() {
            self.rex(false, src.is_ext(), false, base.is_ext());
        }
        self.byte(0x89);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `mov dword [base + disp], src32`: `89 /r` (no `REX.W`) — 32-bit store of
    /// the low dword of `src`.
    pub fn mov_store32_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        if src.is_ext() || base.is_ext() {
            self.rex(false, src.is_ext(), false, base.is_ext());
        }
        self.byte(0x89);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `rep movsb`: `F3 A4` — copy RCX bytes from `[rsi]` to `[rdi]`, advancing
    /// both. Clobbers RSI/RDI/RCX. Used for the larger aggregate `memcpy`.
    pub fn rep_movsb(&mut self) {
        self.byte(0xF3);
        self.byte(0xA4);
    }

    // ---------------------------------------------------------------------
    //  SSE2 scalar-double (f64) family
    // ---------------------------------------------------------------------

    /// Emits the optional REX for an SSE instruction whose `reg`/`rm` may be an
    /// extended xmm register. The mandatory prefix (`F2`/`66`) is emitted by the
    /// caller *before* this REX, then `0F` and the opcode follow.
    fn sse_rex(&mut self, w: bool, reg_ext: bool, base_ext: bool) {
        if w || reg_ext || base_ext {
            self.rex(w, reg_ext, false, base_ext);
        }
    }

    /// `movsd dst, [base + disp]` (load an f64): `F2 0F 10 /r`.
    pub fn movsd_load(&mut self, dst: crate::reg::Xmm, base: Gpr, disp: i32) {
        self.byte(0xF2);
        self.sse_rex(false, dst.is_ext(), base.is_ext());
        self.byte(0x0F);
        self.byte(0x10);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsd [base + disp], src` (store an f64): `F2 0F 11 /r`.
    pub fn movsd_store(&mut self, base: Gpr, disp: i32, src: crate::reg::Xmm) {
        self.byte(0xF2);
        self.sse_rex(false, src.is_ext(), base.is_ext());
        self.byte(0x0F);
        self.byte(0x11);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `movsd dst, src` (xmm→xmm copy): `F2 0F 10 /r` with a register-direct
    /// ModRM (`dst` in reg, `src` in rm).
    pub fn movsd_rr(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.byte(0xF2);
        self.sse_rex(false, dst.is_ext(), src.is_ext());
        self.byte(0x0F);
        self.byte(0x10);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// The shared `F2 0F <op> /r` register-direct scalar-double ALU shape
    /// (`dst` in reg, `src` in rm).
    fn sse_alu_rr(&mut self, op: u8, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.byte(0xF2);
        self.sse_rex(false, dst.is_ext(), src.is_ext());
        self.byte(0x0F);
        self.byte(op);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `addsd dst, src`: `F2 0F 58 /r`.
    pub fn addsd(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.sse_alu_rr(0x58, dst, src);
    }
    /// `subsd dst, src`: `F2 0F 5C /r`.
    pub fn subsd(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.sse_alu_rr(0x5C, dst, src);
    }
    /// `mulsd dst, src`: `F2 0F 59 /r`.
    pub fn mulsd(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.sse_alu_rr(0x59, dst, src);
    }
    /// `divsd dst, src`: `F2 0F 5E /r`.
    pub fn divsd(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.sse_alu_rr(0x5E, dst, src);
    }

    /// `ucomisd dst, src`: `66 0F 2E /r` — an ordered f64 compare that sets the
    /// ZF/PF/CF flags a `setcc`/`jcc` then reads.
    pub fn ucomisd(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.byte(0x66);
        self.sse_rex(false, dst.is_ext(), src.is_ext());
        self.byte(0x0F);
        self.byte(0x2E);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `movq dst, src` (GPR→xmm bit-copy): `66 REX.W 0F 6E /r` — moves the raw
    /// 64-bit bit pattern of GPR `src` into the low quadword of xmm `dst` (zeroing
    /// the high quadword). Used to materialize an `f64` *constant* directly from a
    /// GPR holding its bit pattern, avoiding any stack-memory round trip (and so
    /// never aliasing the outgoing-args region). The mandatory `66` prefix is
    /// emitted before REX, matching the SSE encoding rule the other methods follow.
    pub fn movq_xmm_r64(&mut self, dst: crate::reg::Xmm, src: Gpr) {
        self.byte(0x66);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0x6E);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `cvtsi2sd dst, src` (int→f64): `F2 REX.W 0F 2A /r` — converts the 64-bit
    /// GPR `src` to a double in xmm `dst`.
    pub fn cvtsi2sd(&mut self, dst: crate::reg::Xmm, src: Gpr) {
        self.byte(0xF2);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0x2A);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `cvttsd2si dst, src` (f64→int, truncating): `F2 REX.W 0F 2C /r` — converts
    /// the double in xmm `src` to a 64-bit GPR `dst`, rounding toward zero.
    pub fn cvttsd2si(&mut self, dst: Gpr, src: crate::reg::Xmm) {
        self.byte(0xF2);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0x2C);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `mov dst, imm64` whose 8 immediate bytes are a `.rodata` pointer hole.
    /// Records a [`FixupKind::Data`] so the layout pass writes the string's
    /// absolute virtual address. Always uses the full `B8+rd io` form so the
    /// hole is a fixed 8 bytes regardless of the (unknown) address value.
    pub fn mov_ri_data(&mut self, dst: Gpr, data_off: u32) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xB8 | dst.low3());
        let at = self.pos();
        self.imm64(0);
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::Data(data_off),
        });
    }

    // ---------------------------------------------------------------------
    //  Integer arithmetic / bitwise (r64, r64)
    // ---------------------------------------------------------------------

    /// `add dst, src`: `REX.W 01 /r`.
    pub fn add_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x01, dst, src);
    }
    /// `sub dst, src`: `REX.W 29 /r`.
    pub fn sub_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x29, dst, src);
    }
    /// `and dst, src`: `REX.W 21 /r`.
    pub fn and_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x21, dst, src);
    }
    /// `or dst, src`: `REX.W 09 /r`.
    pub fn or_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x09, dst, src);
    }
    /// `xor dst, src`: `REX.W 31 /r`.
    pub fn xor_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x31, dst, src);
    }
    /// `adc dst, src`: `REX.W 11 /r` (add-with-carry — `dst = dst + src + CF`).
    /// Forms the high limb of a two-limb 128-bit add: an `add_rr` of the low
    /// limbs sets CF, and this `adc_rr` of the high limbs folds it in.
    pub fn adc_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x11, dst, src);
    }
    /// `sbb dst, src`: `REX.W 19 /r` (subtract-with-borrow — `dst = dst - src -
    /// CF`). Forms the high limb of a two-limb 128-bit subtract: a `sub_rr` of
    /// the low limbs sets CF (the borrow), and this `sbb_rr` of the high limbs
    /// propagates it.
    pub fn sbb_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x19, dst, src);
    }
    /// `cmp dst, src`: `REX.W 39 /r` (the `CMP r/m64, r64` direction, so flags
    /// are set as for `dst - src`).
    pub fn cmp_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x39, dst, src);
    }
    /// `test dst, src`: `REX.W 85 /r` (sets ZF for `dst & src == 0`).
    pub fn test_rr(&mut self, dst: Gpr, src: Gpr) {
        self.alu_rr(0x85, dst, src);
    }

    /// The shared `OP r/m64, r64` shape (`src` in the reg field, `dst` in rm).
    fn alu_rr(&mut self, opcode: u8, dst: Gpr, src: Gpr) {
        self.rex(true, src.is_ext(), false, dst.is_ext());
        self.byte(opcode);
        self.modrm_rr(src, dst);
    }

    /// `imul dst, src`: `REX.W 0F AF /r` (`dst *= src`, two-operand form).
    pub fn imul_rr(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xAF);
        self.modrm_rr(dst, src);
    }

    /// `cqo`: `REX.W 99` — sign-extend RAX into RDX:RAX (the dividend setup for
    /// a signed `idiv`).
    pub fn cqo(&mut self) {
        self.rex(true, false, false, false);
        self.byte(0x99);
    }

    /// `idiv src`: `REX.W F7 /7` — signed divide RDX:RAX by `src`, quotient in
    /// RAX and remainder in RDX.
    pub fn idiv_r(&mut self, src: Gpr) {
        self.rex(true, false, false, src.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(7, src);
    }

    /// `div src`: `REX.W F7 /6` — **unsigned** divide RDX:RAX by `src`, quotient
    /// in RAX and remainder in RDX. Used for `u64`/`usize` division, where the
    /// dividend must be treated as a non-negative magnitude (the high half, RDX,
    /// is zeroed by the caller via `xor rdx, rdx` rather than sign-extended by
    /// `cqo`); a signed `idiv` would misinterpret a high-bit-set value as
    /// negative and either compute the wrong quotient or `#DE`-fault.
    pub fn div_r(&mut self, src: Gpr) {
        self.rex(true, false, false, src.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(6, src);
    }

    /// `mul src`: `REX.W F7 /4` — **unsigned** multiply RAX by `src`, producing
    /// the 128-bit product in RDX:RAX. The unsigned overflow check reads CF/OF:
    /// both are set iff the high half (RDX) is nonzero, i.e. the product does not
    /// fit 64 bits.
    pub fn mul_r(&mut self, src: Gpr) {
        self.rex(true, false, false, src.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(4, src);
    }

    /// `imul src`: `REX.W F7 /5` — **signed** one-operand multiply RAX by `src`,
    /// producing the full 128-bit product in RDX:RAX and setting OF iff the
    /// result does not fit a sign-extended 64 bits. Distinct from the two-operand
    /// `imul_rr` (`0F AF`), whose result is truncated to 64 bits; this form is
    /// used by the signed 64-bit overflow predicate so OF reflects the true
    /// 128-bit product.
    pub fn imul_r1(&mut self, src: Gpr) {
        self.rex(true, false, false, src.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(5, src);
    }

    /// `xor rdx, rdx`: zero RDX (the unsigned-division high-half dividend). A
    /// convenience wrapper over `xor_rr` documenting its single use site.
    pub fn zero_rdx(&mut self) {
        self.xor_rr(Gpr::Rdx, Gpr::Rdx);
    }

    /// `neg dst`: `REX.W F7 /3` (two's-complement negation).
    pub fn neg_r(&mut self, dst: Gpr) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(3, dst);
    }

    /// `not dst`: `REX.W F7 /2` (one's-complement / bitwise NOT).
    pub fn not_r(&mut self, dst: Gpr) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(2, dst);
    }

    // ---------------------------------------------------------------------
    //  Shifts (by CL)
    // ---------------------------------------------------------------------

    /// `shl dst, cl`: `REX.W D3 /4`.
    pub fn shl_cl(&mut self, dst: Gpr) {
        self.shift_cl(4, dst);
    }
    /// `shr dst, cl`: `REX.W D3 /5` (logical / unsigned right shift).
    pub fn shr_cl(&mut self, dst: Gpr) {
        self.shift_cl(5, dst);
    }
    /// `sar dst, cl`: `REX.W D3 /7` (arithmetic / signed right shift).
    pub fn sar_cl(&mut self, dst: Gpr) {
        self.shift_cl(7, dst);
    }

    /// The shared `D3 /digit` shift-by-CL shape.
    fn shift_cl(&mut self, op_ext: u8, dst: Gpr) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xD3);
        self.modrm_rr_op(op_ext, dst);
    }

    // ---------------------------------------------------------------------
    //  Immediate ALU (used for width masks)
    // ---------------------------------------------------------------------

    /// `and dst, imm32`: `REX.W 81 /4 id` (sign-extended). Used to mask an
    /// unsigned narrow-width result down to `2^w - 1`.
    pub fn and_ri(&mut self, dst: Gpr, imm: i32) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(4, dst);
        self.imm32(imm);
    }

    /// `xor dst, imm32`: `REX.W 81 /6 id`. Used to flip a boolean (`xor rax, 1`).
    pub fn xor_ri(&mut self, dst: Gpr, imm: i32) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(6, dst);
        self.imm32(imm);
    }

    /// `cmp dst, imm32`: `REX.W 81 /7 id`. Used by the `Switch` compare chain.
    pub fn cmp_ri(&mut self, dst: Gpr, imm: i32) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(7, dst);
        self.imm32(imm);
    }

    /// `add dst, imm32`: `REX.W 81 /0 id` (sign-extended). Folds a field offset
    /// or a scaled index into an address register.
    pub fn add_ri(&mut self, dst: Gpr, imm: i32) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(0, dst);
        self.imm32(imm);
    }

    /// `imul dst, src, imm32`: `REX.W 69 /r id` — `dst = src * imm32`. Computes a
    /// scaled element index (`index * elem_size`) for a non-power-of-two stride.
    pub fn imul_rri(&mut self, dst: Gpr, src: Gpr, imm: i32) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x69);
        self.modrm_rr(dst, src);
        self.imm32(imm);
    }

    /// `shl dst, imm8`: `REX.W C1 /4 ib` — left-shift by a constant (a
    /// power-of-two element-size scale).
    pub fn shl_ri(&mut self, dst: Gpr, imm: u8) {
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xC1);
        self.modrm_rr_op(4, dst);
        self.byte(imm);
    }

    /// `sub rsp, imm32`: `REX.W 81 /5 id` (allocate stack frame).
    pub fn sub_rsp_imm(&mut self, imm: i32) {
        self.rex(true, false, false, false);
        self.byte(0x81);
        self.modrm_rr_op(5, Gpr::Rsp);
        self.imm32(imm);
    }

    /// `add rsp, imm32`: `REX.W 81 /0 id` (deallocate stack frame).
    pub fn add_rsp_imm(&mut self, imm: i32) {
        self.rex(true, false, false, false);
        self.byte(0x81);
        self.modrm_rr_op(0, Gpr::Rsp);
        self.imm32(imm);
    }

    // ---------------------------------------------------------------------
    //  Compare -> bool materialization & width fixups
    // ---------------------------------------------------------------------

    /// `setcc al`: `0F 90+tttn C0` — set AL to 1 if `cc` holds, else 0. (No REX:
    /// AL is the low byte of RAX, register 0.)
    pub fn setcc_al(&mut self, cc: Cc) {
        self.byte(0x0F);
        self.byte(0x90 | cc.tttn());
        self.byte(0xC0);
    }

    /// `movzx dst, al`: `REX.W 0F B6 /r` — zero-extend AL (the `setcc` byte
    /// result) into a full 64-bit `dst`.
    pub fn movzx_al(&mut self, dst: Gpr) {
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x0F);
        self.byte(0xB6);
        // reg = dst, rm = al (register 0).
        self.byte(0b11_000_000 | (dst.low3() << 3));
    }

    /// `movzx dst, src8`: `REX.W 0F B6 /r` — zero-extend the low byte of `src`.
    pub fn movzx8(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xB6);
        self.modrm_rr(dst, src);
    }
    /// `movsx dst, src8`: `REX.W 0F BE /r` — sign-extend the low byte of `src`.
    pub fn movsx8(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xBE);
        self.modrm_rr(dst, src);
    }
    /// `movzx dst, src16`: `REX.W 0F B7 /r` — zero-extend the low word of `src`.
    pub fn movzx16(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xB7);
        self.modrm_rr(dst, src);
    }
    /// `movsx dst, src16`: `REX.W 0F BF /r` — sign-extend the low word of `src`.
    pub fn movsx16(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xBF);
        self.modrm_rr(dst, src);
    }
    /// `movsxd dst, src32`: `REX.W 63 /r` — sign-extend the low dword of `src`.
    pub fn movsxd(&mut self, dst: Gpr, src: Gpr) {
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x63);
        self.modrm_rr(dst, src);
    }

    // ---------------------------------------------------------------------
    //  Stack & control flow
    // ---------------------------------------------------------------------

    /// `push reg`: `50+rd` (plus a `REX.B` prefix for `r8`–`r15`).
    pub fn push(&mut self, reg: Gpr) {
        if reg.is_ext() {
            self.rex(false, false, false, true);
        }
        self.byte(0x50 | reg.low3());
    }

    /// `pop reg`: `58+rd` (plus a `REX.B` prefix for `r8`–`r15`).
    pub fn pop(&mut self, reg: Gpr) {
        if reg.is_ext() {
            self.rex(false, false, false, true);
        }
        self.byte(0x58 | reg.low3());
    }

    /// `ret`: `C3`.
    pub fn ret(&mut self) {
        self.byte(0xC3);
    }

    /// `leave`: `C9` — `mov rsp, rbp; pop rbp` in one byte (the epilogue).
    pub fn leave(&mut self) {
        self.byte(0xC9);
    }

    /// `syscall`: `0F 05`.
    pub fn syscall(&mut self) {
        self.byte(0x0F);
        self.byte(0x05);
    }

    /// `jmp label` (near, rel32): `E9 cd`. Writes a 4-byte placeholder and a
    /// [`FixupKind::Local`] fixup [`Asm::finish`] resolves.
    pub fn jmp(&mut self, label: LabelId) {
        self.byte(0xE9);
        self.push_rel32_fixup(FixupKind::Local(label));
    }

    /// `jcc label` (near, rel32): `0F 80+tttn cd`.
    pub fn jcc(&mut self, cc: Cc, label: LabelId) {
        self.byte(0x0F);
        self.byte(0x80 | cc.tttn());
        self.push_rel32_fixup(FixupKind::Local(label));
    }

    /// `call func` (near, rel32): `E8 cd`. Cross-function — records a
    /// [`FixupKind::Call`] the program layout pass patches.
    pub fn call_fn(&mut self, func: crate::mir_ids::FnId) {
        self.byte(0xE8);
        self.push_rel32_fixup(FixupKind::Call(func));
    }

    /// Writes a 4-byte zero placeholder at the current position and records a
    /// fixup of `kind` pointing at it.
    fn push_rel32_fixup(&mut self, kind: FixupKind) {
        let at = self.pos();
        self.imm32(0);
        self.fixups.push(Fixup { at, kind });
    }

    // ---------------------------------------------------------------------
    //  Labels & finalization
    // ---------------------------------------------------------------------

    /// Reserves `n` distinct labels (returning the first valid id is implicit:
    /// labels are `LabelId(0)..LabelId(n)`). Must be called before any
    /// `bind_label`/`jmp`/`jcc` so the offset table is sized.
    pub fn reserve_labels(&mut self, n: usize) {
        self.label_offsets = vec![None; n];
    }

    /// Records that `label` is at the current code position.
    pub fn bind_label(&mut self, label: LabelId) {
        let i = label.0 as usize;
        debug_assert!(
            i < self.label_offsets.len(),
            "label id out of reserved range"
        );
        self.label_offsets[i] = Some(self.pos());
    }

    /// Allocates a fresh, dynamically-created local label (used by the runtime
    /// print formatter's render loops, which are emitted inline within a block and
    /// need their own jump targets beyond the per-basic-block reserved labels).
    /// The returned [`LabelId`] is resolved by [`Asm::finish`] exactly like a
    /// block label.
    pub fn new_local_label(&mut self) -> LabelId {
        let id = LabelId(self.label_offsets.len() as u32);
        self.label_offsets.push(None);
        id
    }

    /// Binds a dynamically-allocated local label at the current position.
    pub fn bind_local(&mut self, label: LabelId) {
        self.label_offsets[label.0 as usize] = Some(self.pos());
    }

    /// An unconditional jump to a local label (alias of [`Asm::jmp`], named for the
    /// render-loop call sites).
    pub fn jmp_local(&mut self, label: LabelId) {
        self.jmp(label);
    }

    /// A conditional jump to a local label (alias of [`Asm::jcc`]).
    pub fn jcc_local(&mut self, cc: Cc, label: LabelId) {
        self.jcc(cc, label);
    }

    /// Finalizes this function's code: resolves every intra-function
    /// [`FixupKind::Local`] `rel32` in place and returns the code bytes together
    /// with the *surviving* cross-function fixups (`Call`/`Data`) for the program
    /// layout pass to patch. Panics only on a backend bug (an unbound label),
    /// never on subset-valid input — every label is bound when its block is
    /// emitted.
    pub fn finish(mut self) -> (Vec<u8>, Vec<Fixup>) {
        let mut remaining = Vec::new();
        for fx in std::mem::take(&mut self.fixups) {
            match fx.kind {
                FixupKind::Local(label) => {
                    let target = self.label_offsets[label.0 as usize]
                        .expect("every local label must be bound before finish()");
                    let rel = (target as i64) - (fx.at as i64 + 4);
                    let rel32 = rel as i32;
                    self.buf[fx.at..fx.at + 4].copy_from_slice(&rel32.to_le_bytes());
                }
                FixupKind::Call(_) | FixupKind::Data(_) => remaining.push(fx),
            }
        }
        (self.buf, remaining)
    }
}
