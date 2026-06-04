//! The x86-64 general-purpose register file.
//!
//! v0.14 uses a deliberately tiny register discipline (correctness over speed —
//! a real allocator is v0.15): every MIR local lives in an `[rbp - N]` stack
//! slot, and the body computes through exactly two scratch registers,
//! **RAX** (the primary accumulator / scratch-A) and **RCX** (scratch-B, also
//! the shift-count register, which we exploit for `shl`/`shr`/`sar`). RDX is
//! used transiently for `idiv` (the high half of the dividend / the remainder).
//! RDI/RSI/RDX/RCX/R8/R9 are loaded only at SysV call sites, immediately before
//! the `call`. RBX and R12–R15 are never touched, which sidesteps callee-saved
//! bookkeeping entirely.
//!
//! The single fact this module encodes is the **register number** (`0..=15`)
//! each register carries in the x86-64 instruction format. That number splits
//! into a low 3-bit field (the `reg`/`rm`/`base` slot of a ModRM/SIB byte, or
//! the `+r` opcode embedding) and a high bit (carried by the REX prefix's
//! `R`/`X`/`B` flags). [`Gpr::num`] yields the full number and [`Gpr::is_ext`]
//! reports whether the high bit is set (i.e. it is one of `r8`–`r15`).

/// A 64-bit general-purpose register, named in x86-64 register-number order so
/// `Gpr::Rax as u8 == 0`, `Gpr::Rcx as u8 == 1`, and so on through
/// `Gpr::R15 as u8 == 15`. The discriminant IS the architectural register
/// number, so [`Gpr::num`] is a free cast.
// The full 16-register file is modeled so the encoder's REX.B extension logic
// (`is_ext`/`low3`) is exercisable for every `r8`–`r15`, and so callers can name
// any architectural register by number. v0.14's lowering only touches a handful
// (RAX/RCX/RDX + the SysV arg registers + RSP/RBP), leaving the callee-saved
// RBX/R10–R15 unused *by the lowering*; they remain part of the complete,
// unit-tested register file. The `dead_code` allow documents that deliberate
// completeness rather than masking an accident.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Gpr {
    /// `rax` — register 0. The primary accumulator / scratch-A and the SysV
    /// return register.
    Rax = 0,
    /// `rcx` — register 1. Scratch-B and the shift-count register; SysV arg 4.
    Rcx = 1,
    /// `rdx` — register 2. The `idiv` high half / remainder; SysV arg 3.
    Rdx = 2,
    /// `rbx` — register 3. Callee-saved; unused in v0.14.
    Rbx = 3,
    /// `rsp` — register 4. The stack pointer.
    Rsp = 4,
    /// `rbp` — register 5. The frame pointer (base of every stack slot).
    Rbp = 5,
    /// `rsi` — register 6. SysV arg 2.
    Rsi = 6,
    /// `rdi` — register 7. SysV arg 1.
    Rdi = 7,
    /// `r8` — register 8. SysV arg 5.
    R8 = 8,
    /// `r9` — register 9. SysV arg 6.
    R9 = 9,
    /// `r10` — register 10. Callee-clobbered scratch; unused in v0.14.
    R10 = 10,
    /// `r11` — register 11. Callee-clobbered scratch; unused in v0.14.
    R11 = 11,
    /// `r12` — register 12. Callee-saved; unused in v0.14.
    R12 = 12,
    /// `r13` — register 13. Callee-saved; unused in v0.14.
    R13 = 13,
    /// `r14` — register 14. Callee-saved; unused in v0.14.
    R14 = 14,
    /// `r15` — register 15. Callee-saved; unused in v0.14.
    R15 = 15,
}

impl Gpr {
    /// The architectural register number `0..=15`. Because the enum's
    /// discriminants ARE the register numbers, this is a direct cast.
    pub fn num(self) -> u8 {
        self as u8
    }

    /// The low 3 bits of the register number — the value that lands in a
    /// ModRM/SIB field or a `+r` opcode embedding.
    pub fn low3(self) -> u8 {
        self.num() & 0b111
    }

    /// `true` if this is one of `r8`–`r15` (register number `>= 8`), i.e. its
    /// high bit must be carried by a REX prefix `R`/`X`/`B` flag.
    pub fn is_ext(self) -> bool {
        self.num() >= 8
    }
}

/// The System V AMD64 integer argument registers, in call order:
/// `rdi, rsi, rdx, rcx, r8, r9`. The native backend supports calls of at most
/// six integer arguments (the v0.14 corpus never exceeds one), so a callee's
/// arguments are loaded directly into these registers with no stack spill.
pub const ARG_REGS: [Gpr; 6] = [Gpr::Rdi, Gpr::Rsi, Gpr::Rdx, Gpr::Rcx, Gpr::R8, Gpr::R9];
