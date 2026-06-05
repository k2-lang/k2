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
use crate::runtime::RuntimeFn;

thread_local! {
    /// Whether [`Asm::peephole`] performs any rewrite. Always `true` in normal
    /// operation; a stats/measurement helper flips it off for the *same* program
    /// to obtain the un-peepholed baseline `.text` size (see
    /// [`crate::compile_program_to_elf_stats`]). This is a measurement knob only —
    /// the shipped pipeline always peepholes.
    static PEEPHOLE_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether the peephole is currently enabled on this thread.
pub(crate) fn peephole_enabled() -> bool {
    PEEPHOLE_ON.with(|c| c.get())
}

/// Runs `f` with the peephole forced on (`on=true`) or off (`on=false`),
/// restoring the previous setting afterward. Used only to measure the un-optimized
/// baseline code size against the peepholed size for the same program.
pub(crate) fn with_peephole<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let prev = PEEPHOLE_ON.with(|c| c.replace(on));
    let r = f();
    PEEPHOLE_ON.with(|c| c.set(prev));
    r
}

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
    /// Not-sign (`SF=0`). `tttn = 9`. The non-negative branch of a syscall result:
    /// a successful `mmap` returns a (sign-clear) user address, while a failure
    /// returns `-errno` in `[-4095,-1]` (sign set), so `jns` selects success.
    Ns,
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
            Cc::Ns => 0x9,
        }
    }

    /// The AArch64 4-bit condition field (`cond`), used by `b.cc` and `cset`. The
    /// values are from the ARM ARM (DDI 0487) condition-code table. They differ
    /// from x86's `tttn`; this is the per-target re-expression of the *same*
    /// logical condition the shared lowering selected. `Cc::Ns` (not-sign) maps to
    /// `pl` (N==0), and `Cc::C`/`Cc::B` both map to `hs`/`cs` (carry set) as on
    /// x86, where they share an encoding.
    pub fn cond4(self) -> u8 {
        match self {
            Cc::E => 0b0000,  // EQ  Z==1
            Cc::Ne => 0b0001, // NE  Z==0
            Cc::Ge => 0b1010, // GE  N==V
            Cc::L => 0b1011,  // LT  N!=V
            Cc::G => 0b1100,  // GT  Z==0 && N==V
            Cc::Le => 0b1101, // LE  Z==1 || N!=V
            Cc::C => 0b0010,  // CS/HS  C==1 (unsigned carry / overflow)
            Cc::B => 0b0011,  // CC/LO  C==0 (unsigned below)
            Cc::Ae => 0b0010, // HS  C==1 (unsigned above-or-equal)
            Cc::A => 0b1000,  // HI  C==1 && Z==0
            Cc::Be => 0b1001, // LS  C==0 || Z==1
            Cc::O => 0b0110,  // VS  V==1 (signed overflow)
            Cc::Ns => 0b0101, // PL  N==0
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
    /// A `rel32` displacement to a runtime support routine (an `E8` call). The
    /// program layout pass patches it once the routine's offset within `.text` is
    /// known (the runtime prelude is appended after the user functions). These are
    /// the hand-written heap / capability routines (see [`crate::runtime`]).
    Runtime(RuntimeFn),
    /// An 8-byte absolute pointer into the writable state segment (registry / clock
    /// / RNG) at the given byte offset, patched to `state_vaddr + offset` (a `mov
    /// r64, imm64`). The state segment is a fixed-address `.bss`-like `PT_LOAD`, so
    /// the address is known at link time exactly like a `.rodata` pointer.
    State(u32),
}

/// One unresolved reference embedded in the code stream.
#[derive(Clone, Copy, Debug)]
pub struct Fixup {
    /// The byte offset of the placeholder hole within this function's code.
    pub at: usize,
    /// What the hole resolves to.
    pub kind: FixupKind,
}

/// A peephole classification tag recorded for each emitted instruction. The
/// machine-level peephole pass ([`Asm::peephole`]) only *understands* the small
/// recognized subset below; every other instruction is an [`ITag::Other`]
/// **barrier** the pass never reorders across and treats as reading/writing
/// unknown state, so an unhandled instruction can never enable a rewrite. This
/// conservatism is what makes the pass behavior-preserving by construction.
///
/// A tag carries only the structured facts a rule needs (which registers a move
/// reads/writes, the immediate of a `mov r, imm`, a branch's target label). It is
/// never re-encoded from the tag — the already-emitted bytes are reused verbatim
/// except for the two rewrites that emit fresh bytes (`mov r,0`→`xor r,r`), so the
/// tag and the bytes can never drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ITag {
    /// `mov dst, src` (64-bit reg→reg copy). A `dst==src` self-move is a no-op.
    MovRr { dst: Gpr, src: Gpr },
    /// `mov dst32, src32` — the deliberate 32-bit width-normalizing move. A
    /// `dst==src` form zero-extends bits 32–63 and is **load-bearing**, so it is
    /// tagged distinctly from [`ITag::MovRr`] and never deleted by the self-move
    /// rule.
    MovRr32 { dst: Gpr, src: Gpr },
    /// `mov dst, imm` with the immediate captured (drives the `mov r,0`→`xor` and
    /// dead-store rules; `imm` is `Some` only when it fit the recorded form).
    MovRi { dst: Gpr, imm: i64 },
    /// `xor dst, dst` (a register zeroing that clobbers flags). Recognized so the
    /// `mov r,0`→`xor` rule can re-tag its rewrite.
    XorSelf { dst: Gpr },
    /// `jmp label` (unconditional). Drives jump-to-next / jump-to-jump.
    Jmp { label: LabelId },
    /// `jcc cc, label` (conditional). Drives jump-to-jump (the fall-through edge is
    /// never the jcc itself).
    Jcc { label: LabelId },
    /// A bound label marker (zero bytes). Marks a basic-block boundary.
    Label { label: LabelId },
    /// A control / system instruction that reads **no arithmetic flags**
    /// (`ret`/`leave`/`syscall`/`call`): like [`ITag::Other`] it is a full dataflow
    /// barrier (its register effects are unknown, so no move is deleted across it),
    /// but because it never observes the EFLAGS arithmetic bits — and, for a
    /// `call`, because the callee clobbers the flags by the SysV ABI (CALL *itself*
    /// preserves EFLAGS; the kill is the callee's) — no in-function code after it
    /// can depend on the flags. That lets a preceding `mov r,0` become an `xor`
    /// whose flag clobber is then unobservable.
    CtrlNoFlags,
    /// Any other instruction: an opaque barrier. The peephole stops dataflow
    /// reasoning at it (assumes it reads and writes everything, *including* the
    /// flags) and never deletes, reorders, or rewrites across it.
    Other,
}

/// One recorded instruction boundary: the byte offset where the instruction
/// begins and its peephole classification. The instruction's length is the
/// distance to the next mark (or the buffer end for the last).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mark {
    /// The byte offset of the instruction's first byte within [`Asm::buf`].
    at: usize,
    /// The instruction's peephole tag.
    tag: ITag,
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
    /// One [`Mark`] per emitted instruction (and per bound label), in emission
    /// order. The machine-level peephole pass reads this to delete/rewrite whole
    /// instructions; `finish` then rebuilds `buf`/`fixups`/`label_offsets` from the
    /// surviving marks. Recording is automatic (every public emit method tags
    /// itself), so a directly-built `Asm` — a test, the runtime prelude, the
    /// `_start` shim — is peepholed too.
    marks: Vec<Mark>,
    /// How many `.text` bytes the most recent [`Asm::peephole`] removed (for the
    /// codegen size statistics).
    peephole_saved_bytes: usize,
}

/// One whole instruction during the peephole pass: its already-emitted bytes, its
/// [`ITag`] classification, and any fixups whose holes fall inside it (each as a
/// `(byte offset relative to this instruction's start, kind)` pair). Editing the
/// stream at this granularity means deletion is "drop the element" and every byte
/// offset is re-derived on re-serialization — no `Fixup.at` is threaded by hand.
struct Instr {
    /// The peephole classification.
    tag: ITag,
    /// The exact machine-code bytes (reused verbatim, except for a rule that emits
    /// a fresh shorter encoding).
    bytes: Vec<u8>,
    /// Fixup holes inside this instruction, as `(relative offset, kind)`.
    fixups: Vec<(usize, FixupKind)>,
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

    /// Records the start of a new instruction with peephole tag `tag`. Called
    /// exactly once at the head of every public emit method (before any byte is
    /// appended), so `marks` holds one entry per instruction at its first byte. A
    /// bound label records a distinct zero-byte marker at the same offset as the
    /// instruction that follows it — the two coexist (a label is not folded into
    /// the next instruction), so block boundaries survive re-serialization.
    fn mark(&mut self, tag: ITag) {
        self.marks.push(Mark {
            at: self.buf.len(),
            tag,
        });
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
        self.mark(ITag::MovRr { dst, src });
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
        self.mark(ITag::MovRr32 { dst, src });
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
        self.mark(ITag::MovRi { dst, imm });
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
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x8B);
        self.modrm_rbp_disp(dst.low3(), disp);
    }

    /// `mov [rbp + disp], src` (store a stack slot): `REX.W 89 /r`.
    pub fn mov_store(&mut self, disp: i32, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, src.is_ext(), false, false);
        self.byte(0x89);
        self.modrm_rbp_disp(src.low3(), disp);
    }

    /// `lea dst, [rbp + disp]` (address of a stack slot): `REX.W 8D /r`.
    pub fn lea_rbp(&mut self, dst: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x8D);
        self.modrm_rbp_disp(dst.low3(), disp);
    }

    /// `lea dst, [base + disp]` (address of an arbitrary memory operand):
    /// `REX.W 8D /r`. Used to compute the effective address of a projected place
    /// (the base register already holds an interior pointer).
    pub fn lea_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x8D);
        self.modrm_mem(dst.low3(), base, disp);
    }

    // ---------------------------------------------------------------------
    //  Sized loads / stores through an arbitrary base register
    // ---------------------------------------------------------------------

    /// `mov dst, [base + disp]` (64-bit load): `REX.W 8B /r`.
    pub fn mov_load_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x8B);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov [base + disp], src` (64-bit store): `REX.W 89 /r`.
    pub fn mov_store_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, src.is_ext(), false, base.is_ext());
        self.byte(0x89);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `movzx dst, byte [base + disp]`: `REX.W 0F B6 /r` (zero-extend a byte).
    pub fn movzx8_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xB6);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsx dst, byte [base + disp]`: `REX.W 0F BE /r` (sign-extend a byte).
    pub fn movsx8_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xBE);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movzx dst, word [base + disp]`: `REX.W 0F B7 /r` (zero-extend a word).
    pub fn movzx16_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xB7);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsx dst, word [base + disp]`: `REX.W 0F BF /r` (sign-extend a word).
    pub fn movsx16_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x0F);
        self.byte(0xBF);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov dst32, [base + disp]` (32-bit load, **no** `REX.W`): `8B /r`. The CPU
    /// zero-extends bits 32–63, the correct way to load an unsigned 32-bit field.
    pub fn mov_load32_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        if dst.is_ext() || base.is_ext() {
            self.rex(false, dst.is_ext(), false, base.is_ext());
        }
        self.byte(0x8B);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsxd dst, [base + disp]` (32-bit sign-extending load): `REX.W 63 /r`.
    pub fn movsxd_mem(&mut self, dst: Gpr, base: Gpr, disp: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, base.is_ext());
        self.byte(0x63);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `mov byte [base + disp], src8`: `88 /r` — stores the low byte of `src`.
    /// A REX prefix is emitted whenever the base is extended OR `src` is one of
    /// `rsp`/`rbp`/`rsi`/`rdi`/`r8`–`r15`, because without REX the `88 /r` reg
    /// field `4..7` would name `ah/ch/dh/bh` rather than `spl/bpl/sil/dil`.
    pub fn mov_store8_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        self.mark(ITag::Other);
        if src.is_ext() || base.is_ext() || src.num() >= 4 {
            self.rex(false, src.is_ext(), false, base.is_ext());
        }
        self.byte(0x88);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `mov word [base + disp], src16`: `66 89 /r` (the `66` operand-size prefix
    /// selects a 16-bit store of the low word of `src`).
    pub fn mov_store16_mem(&mut self, base: Gpr, disp: i32, src: Gpr) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        if src.is_ext() || base.is_ext() {
            self.rex(false, src.is_ext(), false, base.is_ext());
        }
        self.byte(0x89);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `rep movsb`: `F3 A4` — copy RCX bytes from `[rsi]` to `[rdi]`, advancing
    /// both. Clobbers RSI/RDI/RCX. Used for the larger aggregate `memcpy`.
    pub fn rep_movsb(&mut self) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.byte(0xF2);
        self.sse_rex(false, dst.is_ext(), base.is_ext());
        self.byte(0x0F);
        self.byte(0x10);
        self.modrm_mem(dst.low3(), base, disp);
    }

    /// `movsd [base + disp], src` (store an f64): `F2 0F 11 /r`.
    pub fn movsd_store(&mut self, base: Gpr, disp: i32, src: crate::reg::Xmm) {
        self.mark(ITag::Other);
        self.byte(0xF2);
        self.sse_rex(false, src.is_ext(), base.is_ext());
        self.byte(0x0F);
        self.byte(0x11);
        self.modrm_mem(src.low3(), base, disp);
    }

    /// `movsd dst, src` (xmm→xmm copy): `F2 0F 10 /r` with a register-direct
    /// ModRM (`dst` in reg, `src` in rm).
    pub fn movsd_rr(&mut self, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.mark(ITag::Other);
        self.byte(0xF2);
        self.sse_rex(false, dst.is_ext(), src.is_ext());
        self.byte(0x0F);
        self.byte(0x10);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// The shared `F2 0F <op> /r` register-direct scalar-double ALU shape
    /// (`dst` in reg, `src` in rm).
    fn sse_alu_rr(&mut self, op: u8, dst: crate::reg::Xmm, src: crate::reg::Xmm) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.byte(0x66);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0x6E);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `cvtsi2sd dst, src` (int→f64): `F2 REX.W 0F 2A /r` — converts the 64-bit
    /// GPR `src` to a double in xmm `dst`.
    pub fn cvtsi2sd(&mut self, dst: crate::reg::Xmm, src: Gpr) {
        self.mark(ITag::Other);
        self.byte(0xF2);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0x2A);
        self.byte(0b11_000_000 | (dst.low3() << 3) | src.low3());
    }

    /// `cvttsd2si dst, src` (f64→int, truncating): `F2 REX.W 0F 2C /r` — converts
    /// the double in xmm `src` to a 64-bit GPR `dst`, rounding toward zero.
    pub fn cvttsd2si(&mut self, dst: Gpr, src: crate::reg::Xmm) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xB8 | dst.low3());
        let at = self.pos();
        self.imm64(0);
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::Data(data_off),
        });
    }

    /// `mov dst, imm64` whose 8 immediate bytes are a writable-state pointer hole.
    /// Records a [`FixupKind::State`] so the layout pass writes the segment's
    /// absolute virtual address plus `state_off`. Always uses the full `B8+rd io`
    /// form so the hole is a fixed 8 bytes regardless of the (unknown) address.
    pub fn mov_ri_state(&mut self, dst: Gpr, state_off: u32) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xB8 | dst.low3());
        let at = self.pos();
        self.imm64(0);
        self.fixups.push(Fixup {
            at,
            kind: FixupKind::State(state_off),
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
    /// `xor dst, src`: `REX.W 31 /r`. A self-`xor` (`xor r, r`) is the recognized
    /// register-zeroing shape; [`Asm::alu_rr`] tags it [`ITag::XorSelf`] from the
    /// opcode + operands so the peephole can fold a `mov r,0` into it.
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
        // The one recognized ALU shape is a self-`xor` (register zeroing, opcode
        // 0x31); every other ALU op is an opaque barrier for the peephole.
        if opcode == 0x31 && dst == src {
            self.mark(ITag::XorSelf { dst });
        } else {
            self.mark(ITag::Other);
        }
        self.rex(true, src.is_ext(), false, dst.is_ext());
        self.byte(opcode);
        self.modrm_rr(src, dst);
    }

    /// `imul dst, src`: `REX.W 0F AF /r` (`dst *= src`, two-operand form).
    pub fn imul_rr(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xAF);
        self.modrm_rr(dst, src);
    }

    /// `cqo`: `REX.W 99` — sign-extend RAX into RDX:RAX (the dividend setup for
    /// a signed `idiv`).
    pub fn cqo(&mut self) {
        self.mark(ITag::Other);
        self.rex(true, false, false, false);
        self.byte(0x99);
    }

    /// `idiv src`: `REX.W F7 /7` — signed divide RDX:RAX by `src`, quotient in
    /// RAX and remainder in RDX.
    pub fn idiv_r(&mut self, src: Gpr) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.rex(true, false, false, src.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(6, src);
    }

    /// `mul src`: `REX.W F7 /4` — **unsigned** multiply RAX by `src`, producing
    /// the 128-bit product in RDX:RAX. The unsigned overflow check reads CF/OF:
    /// both are set iff the high half (RDX) is nonzero, i.e. the product does not
    /// fit 64 bits.
    pub fn mul_r(&mut self, src: Gpr) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xF7);
        self.modrm_rr_op(3, dst);
    }

    /// `not dst`: `REX.W F7 /2` (one's-complement / bitwise NOT).
    pub fn not_r(&mut self, dst: Gpr) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(4, dst);
        self.imm32(imm);
    }

    /// `xor dst, imm32`: `REX.W 81 /6 id`. Used to flip a boolean (`xor rax, 1`).
    pub fn xor_ri(&mut self, dst: Gpr, imm: i32) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(6, dst);
        self.imm32(imm);
    }

    /// `cmp dst, imm32`: `REX.W 81 /7 id`. Used by the `Switch` compare chain.
    pub fn cmp_ri(&mut self, dst: Gpr, imm: i32) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(7, dst);
        self.imm32(imm);
    }

    /// `add dst, imm32`: `REX.W 81 /0 id` (sign-extended). Folds a field offset
    /// or a scaled index into an address register.
    pub fn add_ri(&mut self, dst: Gpr, imm: i32) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0x81);
        self.modrm_rr_op(0, dst);
        self.imm32(imm);
    }

    /// `imul dst, src, imm32`: `REX.W 69 /r id` — `dst = src * imm32`. Computes a
    /// scaled element index (`index * elem_size`) for a non-power-of-two stride.
    pub fn imul_rri(&mut self, dst: Gpr, src: Gpr, imm: i32) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x69);
        self.modrm_rr(dst, src);
        self.imm32(imm);
    }

    /// `shl dst, imm8`: `REX.W C1 /4 ib` — left-shift by a constant (a
    /// power-of-two element-size scale).
    pub fn shl_ri(&mut self, dst: Gpr, imm: u8) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xC1);
        self.modrm_rr_op(4, dst);
        self.byte(imm);
    }

    /// `shr dst, imm8`: `REX.W C1 /5 ib` — logical right-shift by a constant. Used
    /// by the runtime splitmix64 PRNG step (`z ^ z >> 30/27/31`).
    pub fn shr_ri(&mut self, dst: Gpr, imm: u8) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xC1);
        self.modrm_rr_op(5, dst);
        self.byte(imm);
    }

    /// `sar dst, imm8`: `REX.W C1 /7 ib` — arithmetic (sign-propagating) right-
    /// shift by a constant. Used by the packed-struct bit-field sign-extension
    /// trick (`shl (64-off-w); sar (64-w)`).
    pub fn sar_ri(&mut self, dst: Gpr, imm: u8) {
        self.mark(ITag::Other);
        self.rex(true, false, false, dst.is_ext());
        self.byte(0xC1);
        self.modrm_rr_op(7, dst);
        self.byte(imm);
    }

    /// `sub rsp, imm32`: `REX.W 81 /5 id` (allocate stack frame).
    pub fn sub_rsp_imm(&mut self, imm: i32) {
        self.mark(ITag::Other);
        self.rex(true, false, false, false);
        self.byte(0x81);
        self.modrm_rr_op(5, Gpr::Rsp);
        self.imm32(imm);
    }

    /// `add rsp, imm32`: `REX.W 81 /0 id` (deallocate stack frame).
    pub fn add_rsp_imm(&mut self, imm: i32) {
        self.mark(ITag::Other);
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
        self.mark(ITag::Other);
        self.byte(0x0F);
        self.byte(0x90 | cc.tttn());
        self.byte(0xC0);
    }

    /// `movzx dst, al`: `REX.W 0F B6 /r` — zero-extend AL (the `setcc` byte
    /// result) into a full 64-bit `dst`.
    pub fn movzx_al(&mut self, dst: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, false);
        self.byte(0x0F);
        self.byte(0xB6);
        // reg = dst, rm = al (register 0).
        self.byte(0b11_000_000 | (dst.low3() << 3));
    }

    /// `movzx dst, src8`: `REX.W 0F B6 /r` — zero-extend the low byte of `src`.
    pub fn movzx8(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xB6);
        self.modrm_rr(dst, src);
    }
    /// `movsx dst, src8`: `REX.W 0F BE /r` — sign-extend the low byte of `src`.
    pub fn movsx8(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xBE);
        self.modrm_rr(dst, src);
    }
    /// `movzx dst, src16`: `REX.W 0F B7 /r` — zero-extend the low word of `src`.
    pub fn movzx16(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xB7);
        self.modrm_rr(dst, src);
    }
    /// `movsx dst, src16`: `REX.W 0F BF /r` — sign-extend the low word of `src`.
    pub fn movsx16(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x0F);
        self.byte(0xBF);
        self.modrm_rr(dst, src);
    }
    /// `movsxd dst, src32`: `REX.W 63 /r` — sign-extend the low dword of `src`.
    pub fn movsxd(&mut self, dst: Gpr, src: Gpr) {
        self.mark(ITag::Other);
        self.rex(true, dst.is_ext(), false, src.is_ext());
        self.byte(0x63);
        self.modrm_rr(dst, src);
    }

    // ---------------------------------------------------------------------
    //  Stack & control flow
    // ---------------------------------------------------------------------

    /// `push reg`: `50+rd` (plus a `REX.B` prefix for `r8`–`r15`).
    pub fn push(&mut self, reg: Gpr) {
        self.mark(ITag::Other);
        if reg.is_ext() {
            self.rex(false, false, false, true);
        }
        self.byte(0x50 | reg.low3());
    }

    /// `pop reg`: `58+rd` (plus a `REX.B` prefix for `r8`–`r15`).
    pub fn pop(&mut self, reg: Gpr) {
        self.mark(ITag::Other);
        if reg.is_ext() {
            self.rex(false, false, false, true);
        }
        self.byte(0x58 | reg.low3());
    }

    /// `ret`: `C3`.
    pub fn ret(&mut self) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0xC3);
    }

    /// `leave`: `C9` — `mov rsp, rbp; pop rbp` in one byte (the epilogue).
    pub fn leave(&mut self) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0xC9);
    }

    /// `syscall`: `0F 05`.
    pub fn syscall(&mut self) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0x0F);
        self.byte(0x05);
    }

    /// `jmp label` (near, rel32): `E9 cd`. Writes a 4-byte placeholder and a
    /// [`FixupKind::Local`] fixup [`Asm::finish`] resolves.
    pub fn jmp(&mut self, label: LabelId) {
        self.mark(ITag::Jmp { label });
        self.byte(0xE9);
        self.push_rel32_fixup(FixupKind::Local(label));
    }

    /// `jcc label` (near, rel32): `0F 80+tttn cd`.
    pub fn jcc(&mut self, cc: Cc, label: LabelId) {
        self.mark(ITag::Jcc { label });
        self.byte(0x0F);
        self.byte(0x80 | cc.tttn());
        self.push_rel32_fixup(FixupKind::Local(label));
    }

    /// `call func` (near, rel32): `E8 cd`. Cross-function — records a
    /// [`FixupKind::Call`] the program layout pass patches.
    pub fn call_fn(&mut self, func: crate::mir_ids::FnId) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0xE8);
        self.push_rel32_fixup(FixupKind::Call(func));
    }

    /// `call <runtime routine>` (near, rel32): `E8 cd`. Records a
    /// [`FixupKind::Runtime`] the program layout pass patches once the routine's
    /// `.text` offset is known.
    pub fn call_runtime(&mut self, rt: RuntimeFn) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0xE8);
        self.push_rel32_fixup(FixupKind::Runtime(rt));
    }

    /// `call label` (near, rel32 to a function-local label): `E8 cd`. Resolved by
    /// [`Asm::finish`]. The runtime trap helper uses `call after; <bytes>; after:
    /// pop` to recover the absolute address of inline data bytes.
    pub fn call_label(&mut self, label: LabelId) {
        self.mark(ITag::CtrlNoFlags);
        self.byte(0xE8);
        self.push_rel32_fixup(FixupKind::Local(label));
    }

    /// Appends `bytes` verbatim into the code stream (raw data embedded in `.text`,
    /// e.g. a runtime trap's panic message reached via a `call`/`pop` thunk).
    pub fn emit_bytes(&mut self, bytes: &[u8]) {
        self.mark(ITag::Other);
        self.buf.extend_from_slice(bytes);
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
        self.mark(ITag::Label { label });
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
        self.mark(ITag::Label { label });
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

    /// The number of `.text` bytes the most recent [`Asm::peephole`] removed. Used
    /// by the codegen statistics so the size reduction can be reported and tested.
    pub fn peephole_savings(&self) -> usize {
        self.peephole_saved_bytes
    }

    /// Runs the machine-level peephole pass over the recorded instruction stream.
    ///
    /// The pass is structured as a per-function list of whole instructions (the
    /// already-emitted bytes plus the [`ITag`] classification and any embedded
    /// fixups), rewritten to a fixpoint, then re-serialized into `buf` /
    /// `label_offsets` / `fixups`. Because it edits at instruction granularity and
    /// re-derives every byte offset on re-serialization, no `Fixup.at` or label
    /// offset is ever threaded by hand — deletion just drops an instruction.
    ///
    /// Every rule is behavior-preserving by construction (see the per-rule notes):
    /// each either deletes an instruction whose only effect is provably dead, emits
    /// an instruction computing the identical value, or rewrites control flow to a
    /// target with identical successor semantics. Unrecognized instructions are
    /// [`ITag::Other`] barriers across which no rule reasons.
    ///
    /// Returns silently (a no-op) when no marks were recorded — a directly-built
    /// `Asm` that never marked is left byte-for-byte unchanged.
    pub fn peephole(&mut self) {
        if self.marks.is_empty() || !peephole_enabled() {
            return;
        }
        let mut instrs = self.explode_to_instrs();
        let before: usize = instrs.iter().map(|i| i.bytes.len()).sum();

        // Fixpoint: rerun until no rule fires (rules can cascade — a deleted block
        // turns a jump-to-jump into a jump-to-next, etc.). The instruction count
        // strictly decreases on each change, so this terminates.
        loop {
            let mut changed = false;
            changed |= peep_self_moves(&mut instrs);
            changed |= peep_mov_zero_to_xor(&mut instrs);
            changed |= peep_dead_store(&mut instrs);
            changed |= peep_jump_to_next(&mut instrs);
            changed |= peep_jump_to_jump(&mut instrs);
            if !changed {
                break;
            }
        }

        let after: usize = instrs.iter().map(|i| i.bytes.len()).sum();
        self.peephole_saved_bytes = before.saturating_sub(after);
        self.reassemble_from_instrs(instrs);
    }

    /// Splits the flat byte buffer into a per-instruction list, attaching each
    /// instruction's tag and the fixups whose holes fall within it (recorded as a
    /// byte offset relative to the instruction's start).
    fn explode_to_instrs(&mut self) -> Vec<Instr> {
        let buf = std::mem::take(&mut self.buf);
        let fixups = std::mem::take(&mut self.fixups);
        let marks = std::mem::take(&mut self.marks);

        let mut instrs: Vec<Instr> = Vec::with_capacity(marks.len());
        for (i, m) in marks.iter().enumerate() {
            let end = marks.get(i + 1).map(|n| n.at).unwrap_or(buf.len());
            instrs.push(Instr {
                tag: m.tag,
                bytes: buf[m.at..end].to_vec(),
                fixups: Vec::new(),
            });
        }
        // Bucket each fixup into the instruction that contains its hole.
        for fx in fixups {
            // Find the last mark whose `at <= fx.at` (the instruction it belongs to).
            let idx = match marks.binary_search_by(|m| m.at.cmp(&fx.at)) {
                Ok(i) => i,
                Err(0) => 0,
                Err(i) => i - 1,
            };
            let rel = fx.at - marks[idx].at;
            instrs[idx].fixups.push((rel, fx.kind));
        }
        instrs
    }

    /// Re-serializes the (possibly shrunk) instruction list back into the flat
    /// `buf`, recomputing every `label_offsets` entry and every `Fixup.at` from the
    /// instructions' new positions. Label *ids* are stable (a `Label` instruction
    /// carries its id); only their byte offsets move.
    fn reassemble_from_instrs(&mut self, instrs: Vec<Instr>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut fixups: Vec<Fixup> = Vec::new();
        for inst in &instrs {
            let base = buf.len();
            if let ITag::Label { label } = inst.tag {
                // A label marks the position of the *next* emitted byte.
                self.label_offsets[label.0 as usize] = Some(base);
            }
            buf.extend_from_slice(&inst.bytes);
            for (rel, kind) in &inst.fixups {
                fixups.push(Fixup {
                    at: base + rel,
                    kind: *kind,
                });
            }
        }
        self.buf = buf;
        self.fixups = fixups;
    }

    /// Finalizes this function's code: resolves every intra-function
    /// [`FixupKind::Local`] `rel32` in place and returns the code bytes together
    /// with the *surviving* cross-function fixups (`Call`/`Data`) for the program
    /// layout pass to patch. Panics only on a backend bug (an unbound label),
    /// never on subset-valid input — every label is bound when its block is
    /// emitted.
    ///
    /// The machine-level [`Asm::peephole`] runs first, so the returned bytes are
    /// the minimized stream; the `rel32` resolution below sees the final offsets.
    pub fn finish(mut self) -> (Vec<u8>, Vec<Fixup>) {
        self.peephole();
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
                FixupKind::Call(_)
                | FixupKind::Data(_)
                | FixupKind::Runtime(_)
                | FixupKind::State(_) => remaining.push(fx),
            }
        }
        (self.buf, remaining)
    }
}

// ===========================================================================
//  Machine-level peephole rules.
//
//  Each rule takes the per-function instruction list and rewrites it in place,
//  returning whether it changed anything. Every rule is conservative by
//  construction: it stops dataflow reasoning at an `ITag::Other` barrier (whose
//  reads/writes are unknown) and at a basic-block boundary (a `Label`, into which
//  another edge may jump). A rewrite is applied only when the transformed stream
//  is provably observationally identical to the original.
// ===========================================================================

/// The condition encoding a `Jcc` instruction carries, recovered from its second
/// opcode byte (`0F 80+tttn`). Only needed to re-emit a retargeted `jcc`.
fn jcc_tttn(bytes: &[u8]) -> Option<u8> {
    // `jcc` is `0F 80+tttn <rel32>`.
    if bytes.len() == 6 && bytes[0] == 0x0F && (bytes[1] & 0xF0) == 0x80 {
        Some(bytes[1] & 0x0F)
    } else {
        None
    }
}

/// Rule 1 — **redundant self-move.** Delete every `mov r, r` (a no-op). The
/// deliberate width-normalizing `mov r32, r32` is tagged [`ITag::MovRr32`] and is
/// never matched here, so the load-bearing zero-extension is preserved.
fn peep_self_moves(instrs: &mut Vec<Instr>) -> bool {
    let before = instrs.len();
    instrs.retain(|i| !matches!(i.tag, ITag::MovRr { dst, src } if dst == src));
    instrs.len() != before
}

/// Rule 3 — **dead store to a register.** When a value-only write to register `r`
/// (`mov r, imm` or `mov r, src`) is *immediately* followed by another full write
/// to the same `r` that does not read it, the first write is dead: nothing between
/// them can observe `r` (they are adjacent, so no `Other` barrier and no label
/// boundary sits between), and the second overwrites `r` entirely. Delete the
/// first. Both instructions are side-effect-free register moves, so removing the
/// first changes nothing observable.
fn peep_dead_store(instrs: &mut Vec<Instr>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i + 1 < instrs.len() {
        let first_dst = match instrs[i].tag {
            ITag::MovRi { dst, .. } => Some(dst),
            ITag::MovRr { dst, src } if dst != src => Some(dst),
            _ => None,
        };
        if let Some(r) = first_dst {
            // Does the *next* instruction fully overwrite `r` without reading it?
            let overwrites = match instrs[i + 1].tag {
                ITag::MovRi { dst, .. } => dst == r,
                // `mov r, src` overwrites `r`; it reads `src`, so the source must
                // not be `r` (else the first write feeds the second).
                ITag::MovRr { dst, src } => dst == r && src != r,
                ITag::XorSelf { dst } => dst == r,
                _ => false,
            };
            if overwrites {
                instrs.remove(i);
                changed = true;
                // Re-examine the new instruction now at `i` against its successor.
                continue;
            }
        }
        i += 1;
    }
    changed
}

/// Rule 2 — **`mov r, 0` → `xor r, r`** (7 bytes → 3). `xor` clobbers the flags
/// while `mov` does not, so the rewrite is only behavior-preserving when the flags
/// `xor` would destroy are provably **dead** at the site — i.e. no execution that
/// could observe the live flags reaches a flag reader before something redefines
/// the flags.
///
/// ## Why a same-block forward scan is not enough (the soundness landmine)
///
/// An earlier version treated a `Jmp`, a `Label`, and the end of the function as
/// proof the flags were dead, because no instruction *within the current block*
/// read them. That ignores cross-block flag liveness: a `cmp` can set flags, a
/// `mov r,0` follow, then a `jmp L`, and block `L` open with a `jcc`/`setcc`/`adc`
/// that reads those flags — they are *live-out* across the jump edge. Rewriting
/// the `mov` to `xor` there clobbers the still-live flags and the successor
/// branches on garbage. Today's front-end lowering happens to emit every
/// `cmp/test; jcc` adjacently (no block is *entered* with flags live), but nothing
/// enforces that invariant, and any directly-built `Asm` (or a future lowering
/// that hoists a shared condition across an edge) reintroduces the live-across-edge
/// shape — a latent miscompile.
///
/// ## The sound rule (conservative same-block clobber proof)
///
/// Fire only when, scanning forward **within the same basic block**, a
/// flag-CLOBBERING instruction provably executes before any flag READER and before
/// any block-exit edge. Then whatever the `xor` does to the flags is overwritten
/// before anyone could observe it, regardless of cross-block liveness. A plain
/// `Jmp`/`Label`/end-of-function is *not* such a proof (the flags may be live-out),
/// so the window ends UNSAFE at a block boundary. This makes the rule fire rarely —
/// only the `mov r,0; <flag-neutral moves>; call|ret|syscall|leave|xor` shapes —
/// which is acceptable: correctness outweighs the tiny size win.
fn peep_mov_zero_to_xor(instrs: &mut [Instr]) -> bool {
    let mut changed = false;
    for i in 0..instrs.len() {
        let dst = match instrs[i].tag {
            ITag::MovRi { dst, imm: 0 } => dst,
            _ => continue,
        };
        if flags_clobbered_before_use_in_block(instrs, i + 1) {
            // Re-emit as `xor dst, dst` (`REX.W 31 /r`, register-direct).
            let mut a = Asm::new();
            a.xor_rr(dst, dst);
            instrs[i] = Instr {
                tag: ITag::XorSelf { dst },
                bytes: a.buf,
                fixups: Vec::new(),
            };
            changed = true;
        }
    }
    changed
}

/// True iff, scanning forward from `start`, a flag-CLOBBERING instruction provably
/// executes **before any flag reader and before any block-exit edge** — the only
/// condition under which clobbering the flags with `xor` is unobservable.
///
/// Sound by construction: it never reasons about flag liveness across a block
/// boundary. A `Jmp`/`Label`/end-of-function ends the window UNSAFE (the flags may
/// be live-out to a successor that reads them). Recognized clobbers are `XorSelf`
/// (zeroes flags, reads none) and `CtrlNoFlags` (`call` makes the flags ABI-dead —
/// the callee clobbers them by the SysV contract; `ret`/`syscall`/`leave` leave the
/// function so no in-function reader follows). A `Jcc` or an opaque `Other`
/// (possibly `setcc`/`adc`/`cmovcc`) is a potential flag READER and ends the
/// window UNSAFE *before* any later clobber, so a clobber after them does not
/// count. Only flag-neutral moves may precede the clobber.
fn flags_clobbered_before_use_in_block(instrs: &[Instr], start: usize) -> bool {
    for inst in &instrs[start..] {
        match inst.tag {
            // Flag-neutral moves: keep scanning for a clobber.
            ITag::MovRr { .. } | ITag::MovRr32 { .. } | ITag::MovRi { .. } => {}
            // A clobber with no prior flag read: the `xor`'s effect is overwritten
            // here before anything observes it. SAFE to rewrite.
            ITag::XorSelf { .. } => return true,
            // `call`/`ret`/`syscall`/`leave`. A `call`'s callee clobbers the flags
            // by ABI (CALL itself preserves EFLAGS — the kill is the callee's, not
            // the instruction's); `ret`/`syscall`/`leave` exit the function. Either
            // way no in-function code observes the pre-`xor` flags. SAFE.
            ITag::CtrlNoFlags => return true,
            // Block boundary: control leaves this block with the flags possibly
            // LIVE-OUT to a successor (reached via this jmp, or the fall-through at
            // a label) that may read them. We do not do cross-block liveness, so
            // this is UNSAFE — do not rewrite.
            ITag::Jmp { .. } | ITag::Label { .. } => return false,
            // A conditional jump READS the flags before any clobber — UNSAFE.
            ITag::Jcc { .. } => return false,
            // An opaque instruction may read the flags (`setcc`/`adc`/`cmovcc`/…)
            // before any clobber — conservatively UNSAFE.
            ITag::Other => return false,
        }
    }
    // Reached the function end with no proven clobber: the flags may be live-out
    // (a tail block whose flags feed a caller is not a thing here, but the absence
    // of a clobber means we cannot prove the `xor` is invisible). UNSAFE.
    false
}

/// Rule 6 — **jump-to-next (fall-through).** Delete a `jmp L` immediately followed
/// by the `Label L` it targets: the unconditional branch lands on the very next
/// instruction, so removing it changes nothing. This is the common shape for a
/// `Goto(next_block)` terminator and the dead `jmp else` after a `Branch` whose
/// else-block falls through.
fn peep_jump_to_next(instrs: &mut Vec<Instr>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i + 1 < instrs.len() {
        if let (ITag::Jmp { label: tgt }, ITag::Label { label: here }) =
            (instrs[i].tag, instrs[i + 1].tag)
        {
            if tgt == here {
                instrs.remove(i);
                changed = true;
                continue;
            }
        }
        i += 1;
    }
    changed
}

/// Rule 7 — **jump-to-jump (thread).** Retarget a `jmp`/`jcc` whose target label
/// is bound on an instruction that is *itself* an unconditional `jmp L2` (an empty
/// forwarding block) so it jumps straight to `L2`. Bounded by a visited set to
/// avoid cycling on a self-loop. Control-flow semantics are unchanged: both paths
/// end at `L2` with no instruction executed in between.
fn peep_jump_to_jump(instrs: &mut [Instr]) -> bool {
    // Map every label id to the index of its `Label` instruction.
    let label_at: std::collections::HashMap<u32, usize> = instrs
        .iter()
        .enumerate()
        .filter_map(|(idx, inst)| match inst.tag {
            ITag::Label { label } => Some((label.0, idx)),
            _ => None,
        })
        .collect();

    // The *effective* final target of a label: follow a chain of empty
    // `Label*; jmp L2` forwarding blocks. `None` means "no improvement" (the label
    // is not a pure forwarder, or the chain cycles). Computed up-front against an
    // immutable borrow so the rewrite loop can mutate freely.
    let resolve = |start: LabelId| -> Option<LabelId> {
        let mut cur = start;
        let mut hops = 0;
        loop {
            hops += 1;
            if hops > 16 {
                return None; // cycle / too long: leave as-is.
            }
            let &idx = label_at.get(&cur.0)?;
            // Skip consecutive labels bound at the same spot to find the first real
            // instruction of the block.
            let mut j = idx;
            while j < instrs.len() && matches!(instrs[j].tag, ITag::Label { .. }) {
                j += 1;
            }
            match instrs.get(j).map(|i| i.tag) {
                Some(ITag::Jmp { label: next }) if next != cur => cur = next,
                _ => return if cur == start { None } else { Some(cur) },
            }
        }
    };

    // Phase 1: compute the retarget for each jump under the immutable borrow.
    let retargets: Vec<Option<LabelId>> = instrs
        .iter()
        .map(|inst| match inst.tag {
            ITag::Jmp { label } | ITag::Jcc { label } => resolve(label),
            _ => None,
        })
        .collect();

    // Phase 2: apply the retargets.
    let mut changed = false;
    for (inst, retarget) in instrs.iter_mut().zip(retargets) {
        let Some(t) = retarget else { continue };
        match inst.tag {
            ITag::Jmp { .. } => {
                rewrite_jmp_target(inst, t);
                changed = true;
            }
            ITag::Jcc { .. } => {
                rewrite_jcc_target(inst, t);
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

/// Rewrites an unconditional-jump instruction to a new target label, preserving
/// its single `Local` fixup (the `rel32` hole at byte offset 1).
fn rewrite_jmp_target(inst: &mut Instr, target: LabelId) {
    // `jmp rel32` is `E9 <rel32>`; the fixup hole is the 4 bytes after the opcode.
    *inst = Instr {
        tag: ITag::Jmp { label: target },
        bytes: vec![0xE9, 0, 0, 0, 0],
        fixups: vec![(1, FixupKind::Local(target))],
    };
}

/// Rewrites a conditional-jump instruction to a new target label, preserving its
/// condition code (recovered from the opcode) and its `Local` fixup (offset 2).
fn rewrite_jcc_target(inst: &mut Instr, target: LabelId) {
    let tttn = jcc_tttn(&inst.bytes).expect("a Jcc instruction must be a 6-byte 0F 8x rel32");
    *inst = Instr {
        tag: ITag::Jcc { label: target },
        bytes: vec![0x0F, 0x80 | tttn, 0, 0, 0, 0],
        fixups: vec![(2, FixupKind::Local(target))],
    };
}

#[cfg(test)]
mod peephole_tests {
    //! Byte-level unit tests for each machine-level peephole rule. Each builds a
    //! small `Asm`, runs the pass via `finish()`, and asserts the resulting bytes
    //! are the expected minimized form — and that an `Other` barrier blocks an
    //! unsafe rewrite. These are host-independent (no execution).

    use super::*;
    use crate::reg::Gpr;

    /// `mov rbx, rbx` (a self-move) is deleted; a real `mov rbx, rcx` survives.
    #[test]
    fn rule_self_move_deleted() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_rr(Gpr::Rbx, Gpr::Rbx); // no-op -> deleted
        a.mov_rr(Gpr::Rbx, Gpr::Rcx); // 48 89 cb -> kept
        a.ret();
        let (code, _) = a.finish();
        // Only the real move + ret remain.
        assert_eq!(code, vec![0x48, 0x89, 0xcb, 0xc3]);
    }

    /// `mov r32, r32` (the width-normalizing move) is NOT deleted even when
    /// dst==src: it zero-extends the upper 32 bits and is load-bearing.
    #[test]
    fn rule_self_move_keeps_norm32() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_rr32(Gpr::Rbx, Gpr::Rbx); // 89 db (no REX.W) -> kept
        a.ret();
        let (code, _) = a.finish();
        assert_eq!(code, vec![0x89, 0xdb, 0xc3]);
    }

    /// A `mov r, 0` whose flags are dead until the block end becomes `xor r, r`
    /// (7 bytes -> 3). Here the zero is followed only by another flag-neutral move
    /// and a `ret`, so flags are dead.
    #[test]
    fn rule_mov_zero_to_xor() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_ri(Gpr::Rax, 0); // -> xor rax, rax (48 31 c0)
        a.ret();
        let (code, _) = a.finish();
        assert_eq!(code, vec![0x48, 0x31, 0xc0, 0xc3]);
    }

    /// A `mov r, 0` is NOT rewritten when a flag consumer (`jcc`) could observe the
    /// flags the `xor` would clobber: a conditional jump before the block ends
    /// blocks the rewrite, so the `mov` (which leaves flags untouched) is kept.
    #[test]
    fn rule_mov_zero_to_xor_blocked_by_flag_use() {
        let mut a = Asm::new();
        a.reserve_labels(1);
        a.mov_ri(Gpr::Rax, 0); // flags must survive for the jcc below
        a.jcc(Cc::E, LabelId(0)); // a flag consumer
        a.bind_label(LabelId(0));
        a.ret();
        let (code, _) = a.finish();
        // The mov stays as `48 C7 C0 00 00 00 00` (7 bytes), not an xor.
        assert_eq!(&code[0..3], &[0x48, 0xc7, 0xc0]);
        assert_eq!(&code[3..7], &[0, 0, 0, 0]);
    }

    /// A dead store: `mov rbx, 1` immediately overwritten by `mov rbx, 2` — the
    /// first is deleted.
    #[test]
    fn rule_dead_store() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_ri(Gpr::Rbx, 1); // dead: overwritten before any read
        a.mov_ri(Gpr::Rbx, 2); // -> 48 c7 c3 02 00 00 00
        a.ret();
        let (code, _) = a.finish();
        assert_eq!(code, vec![0x48, 0xc7, 0xc3, 0x02, 0x00, 0x00, 0x00, 0xc3]);
    }

    /// A dead store is NOT removed across an `Other` barrier (which might read the
    /// register). `mov rbx,1; add rbx,rax; mov rbx,2` keeps the first move.
    #[test]
    fn rule_dead_store_blocked_by_barrier() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_ri(Gpr::Rbx, 1);
        a.add_rr(Gpr::Rbx, Gpr::Rax); // Other barrier reads rbx
        a.mov_ri(Gpr::Rbx, 2);
        a.ret();
        let (code, _) = a.finish();
        // The first mov (7 bytes) survives because the barrier may read rbx.
        assert_eq!(&code[0..3], &[0x48, 0xc7, 0xc3]);
        assert!(
            code.len() > 8,
            "first mov must be kept (len {})",
            code.len()
        );
    }

    /// Jump-to-next: a `jmp L` immediately followed by `L:` is deleted (fall-through).
    #[test]
    fn rule_jump_to_next_deleted() {
        let mut a = Asm::new();
        a.reserve_labels(1);
        a.jmp(LabelId(0)); // targets the very next instruction
        a.bind_label(LabelId(0));
        a.ret();
        let (code, _) = a.finish();
        // Only the ret remains.
        assert_eq!(code, vec![0xc3]);
    }

    /// Jump-to-jump: `jmp L1` where `L1:` is itself `jmp L2` threads to `L2`.
    #[test]
    fn rule_jump_to_jump_threaded() {
        let mut a = Asm::new();
        a.reserve_labels(3);
        // jmp L0 ; ret(filler) ; L0: jmp L1 ; ret(filler) ; L1: ret
        a.jmp(LabelId(0));
        a.bind_label(LabelId(2)); // a placeholder so blocks are not adjacent
        a.mov_rr(Gpr::Rax, Gpr::Rcx); // filler so L0 is not the next instr
        a.bind_label(LabelId(0));
        a.jmp(LabelId(1)); // empty forwarding block
        a.mov_rr(Gpr::Rdx, Gpr::Rcx); // filler
        a.bind_label(LabelId(1));
        a.ret();
        let (code, _) = a.finish();
        // The leading `jmp` must now skip straight to L1 (the ret). It is still a
        // 5-byte `E9 rel32`; its rel32 should point at the final ret.
        assert_eq!(code[0], 0xe9);
        let rel = i32::from_le_bytes([code[1], code[2], code[3], code[4]]);
        let target = (5i64 + rel as i64) as usize;
        assert_eq!(
            code[target], 0xc3,
            "threaded jmp must land on the final ret"
        );
    }

    /// `peephole_savings` reports the bytes the pass removed (here: one deleted
    /// self-move = 3 bytes), so the size statistic is exercised by a unit test.
    #[test]
    fn savings_counts_removed_bytes() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_rr(Gpr::Rbx, Gpr::Rbx); // 3-byte no-op -> deleted
        a.ret();
        a.peephole();
        assert_eq!(a.peephole_savings(), 3, "one deleted self-move is 3 bytes");
    }

    /// An `Other` barrier between a self-move pattern does not enable an unsafe
    /// rewrite: a `mov r,0` whose flags are consumed by an opaque `Other` (which
    /// might be `adc`/`setcc`) is conservatively left as a `mov`.
    #[test]
    fn rule_other_is_a_barrier() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_ri(Gpr::Rax, 0);
        a.adc_rr(Gpr::Rax, Gpr::Rcx); // Other: reads CF (a flag) — blocks the xor rewrite
        a.ret();
        let (code, _) = a.finish();
        assert_eq!(
            &code[0..3],
            &[0x48, 0xc7, 0xc0],
            "mov r,0 must survive before adc"
        );
    }

    /// Soundness counterexample (finding v0.17-#3): a `mov r,0` whose flags are
    /// LIVE-OUT across an unconditional `jmp` to a block that opens with a `jcc`
    /// must NOT become `xor r,r`. The producing `cmp` sets the flags, the `mov`
    /// follows, then `jmp L` leaves the block; block `L` reads the flags with a
    /// `jcc`. An `xor` here would clobber the still-live flags and the `jcc` would
    /// branch on garbage. The old same-block scan treated the `jmp` as "flags
    /// dead" and miscompiled this shape; the fixed rule ends the window UNSAFE at
    /// the block boundary, so the 7-byte `mov` is preserved.
    #[test]
    fn rule_mov_zero_to_xor_unsound_across_jmp_is_blocked() {
        let mut a = Asm::new();
        a.reserve_labels(2);
        a.cmp_rr(Gpr::Rax, Gpr::Rcx); // sets flags that are LIVE across the jmp
        a.mov_ri(Gpr::Rbx, 0); // candidate: must stay a `mov`, not become `xor`
        a.jmp(LabelId(0)); // leaves the block with flags live-out
        a.mov_rr(Gpr::Rdx, Gpr::Rcx); // filler block (not reached with flags live)
        a.bind_label(LabelId(0));
        a.jcc(Cc::E, LabelId(1)); // reads the flags set by the cmp above
        a.bind_label(LabelId(1));
        a.ret();
        let (code, _) = a.finish();
        // The `mov rbx,0` (7 bytes: 48 C7 C3 00 00 00 00) must survive verbatim —
        // an `xor rbx,rbx` (48 31 DB) would have clobbered the flags the jcc reads.
        let mov = [0x48u8, 0xc7, 0xc3, 0x00, 0x00, 0x00, 0x00];
        assert!(
            code.windows(mov.len()).any(|w| w == mov),
            "mov rbx,0 must survive (flags live-out across the jmp); got {code:02x?}"
        );
        let xor = [0x48u8, 0x31, 0xdb];
        assert!(
            !code.windows(xor.len()).any(|w| w == xor),
            "must NOT emit `xor rbx,rbx` across a flag-live block edge; got {code:02x?}"
        );
    }

    /// The companion SAFE shape: a `mov r,0` whose flags are clobbered within the
    /// same block (here by a `syscall`, after which the kernel/ABI leaves the flags
    /// unobservable — the same rationale that makes `call` safe) before any reader
    /// or block exit IS still rewritten to the 3-byte `xor`. This pins that the
    /// soundness fix did not over-tighten the common rewritable shape away.
    #[test]
    fn rule_mov_zero_to_xor_fires_when_clobbered_in_block() {
        let mut a = Asm::new();
        a.reserve_labels(0);
        a.mov_ri(Gpr::Rbx, 0); // -> xor rbx,rbx (CtrlNoFlags follows in-block)
        a.syscall(); // CtrlNoFlags: ends the flag-liveness window
        a.ret();
        let (code, _) = a.finish();
        let xor = [0x48u8, 0x31, 0xdb];
        assert!(
            code.windows(xor.len()).any(|w| w == xor),
            "mov rbx,0 should become `xor rbx,rbx` before a syscall; got {code:02x?}"
        );
    }
}
