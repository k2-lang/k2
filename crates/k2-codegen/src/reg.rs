//! The x86-64 general-purpose register file.
//!
//! v0.15 splits the file into fixed scratch and an allocatable pool. The lowering
//! threads arithmetic operands through **RAX**/**RCX** (RCX also being the shift
//! count) with **RDX** for `idiv`/remainder, and reserves **R11** as an address /
//! `memcpy` / print-cursor scratch; **RDI/RSI/RDX/RCX/R8/R9** carry SysV call
//! arguments and the print-render code freely clobbers the caller-saved set. The
//! linear-scan allocator ([`crate::regalloc`]) assigns the **callee-saved** pool
//! (`RBX`, `R12`–`R15`) to long-lived scalar locals, saving/restoring them once in
//! the prologue/epilogue; values it cannot place spill to a stack home.
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
// any architectural register by number. The lowering and the allocator together
// touch most of the file; the `dead_code` allow covers the few enum variants
// (e.g. `R10`) the current lowering never names by value, keeping the complete,
// unit-tested register file intact rather than masking an accident.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Gpr {
    /// `rax` — register 0. The primary accumulator / scratch-A and the SysV
    /// return register.
    Rax = 0,
    /// `rcx` — register 1. Scratch-B and the shift-count register; SysV arg 4.
    Rcx = 1,
    /// `rdx` — register 2. The `idiv` high half / remainder; SysV arg 3.
    Rdx = 2,
    /// `rbx` — register 3. Callee-saved; an allocatable vreg register.
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
    /// `r10` — register 10. Caller-saved scratch.
    R10 = 10,
    /// `r11` — register 11. The reserved address / memcpy / print-cursor scratch.
    R11 = 11,
    /// `r12` — register 12. Callee-saved; an allocatable vreg register.
    R12 = 12,
    /// `r13` — register 13. Callee-saved; an allocatable vreg register.
    R13 = 13,
    /// `r14` — register 14. Callee-saved; an allocatable vreg register.
    R14 = 14,
    /// `r15` — register 15. Callee-saved; an allocatable vreg register.
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
/// `rdi, rsi, rdx, rcx, r8, r9`. Arguments beyond the sixth integer go on the
/// stack (the v0.15 backend handles `>6` args via the reserved outgoing-args
/// region of the caller's frame).
pub const ARG_REGS: [Gpr; 6] = [Gpr::Rdi, Gpr::Rsi, Gpr::Rdx, Gpr::Rcx, Gpr::R8, Gpr::R9];

/// The integer registers the linear-scan allocator may assign to a vreg.
///
/// v0.15 allocates **only callee-saved** registers (`rbx`, `r12`–`r15`). They
/// survive a `call`/`syscall` automatically (no per-call save/restore traffic),
/// which keeps the allocator correctness-first: the lowering's scratch /
/// print-render / `memcpy` code freely clobbers every caller-saved register
/// (`rax`/`rcx`/`rdx`/`rsi`/`rdi`/`r8`–`r11`) without disturbing any vreg. A
/// future milestone can widen this pool with spill-around-call logic for the
/// caller-saved registers; here a value the allocator cannot place in a
/// callee-saved register simply spills to a stack home.
///
/// Deliberately excluded: `rax`/`rcx`/`rdx` (fixed arithmetic/`idiv`/`shift`
/// scratch), `rsi`/`rdi`/`r8`–`r11` (caller-saved scratch the render/`memcpy`
/// code clobbers), and `rsp`/`rbp`.
pub const ALLOC_REGS: [Gpr; 5] = [Gpr::Rbx, Gpr::R12, Gpr::R13, Gpr::R14, Gpr::R15];

/// `true` if `r` is a caller-saved (volatile) register — clobbered across a
/// `call`/`syscall`, so a vreg living in one must be spilled around a call.
pub fn is_caller_saved(r: Gpr) -> bool {
    !matches!(
        r,
        Gpr::Rbx | Gpr::R12 | Gpr::R13 | Gpr::R14 | Gpr::R15 | Gpr::Rsp | Gpr::Rbp
    )
}

/// An x86-64 SSE register (`xmm0`–`xmm15`). The discriminant IS the architectural
/// register number, so the encoder reads `low3`/`is_ext` exactly as for [`Gpr`].
#[allow(dead_code)] // The full 16-register file is modeled; the f64 lowering uses a subset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Xmm {
    /// `xmm0` — SSE arg/return register 0.
    Xmm0 = 0,
    /// `xmm1`.
    Xmm1 = 1,
    /// `xmm2`.
    Xmm2 = 2,
    /// `xmm3`.
    Xmm3 = 3,
    /// `xmm4`.
    Xmm4 = 4,
    /// `xmm5`.
    Xmm5 = 5,
    /// `xmm6`.
    Xmm6 = 6,
    /// `xmm7` — the last SSE arg register.
    Xmm7 = 7,
    /// `xmm8`.
    Xmm8 = 8,
    /// `xmm9`.
    Xmm9 = 9,
    /// `xmm10`.
    Xmm10 = 10,
    /// `xmm11`.
    Xmm11 = 11,
    /// `xmm12`.
    Xmm12 = 12,
    /// `xmm13`.
    Xmm13 = 13,
    /// `xmm14`.
    Xmm14 = 14,
    /// `xmm15`.
    Xmm15 = 15,
}

impl Xmm {
    /// The architectural register number `0..=15`.
    pub fn num(self) -> u8 {
        self as u8
    }
    /// The low 3 bits (the ModRM `reg`/`rm` field).
    pub fn low3(self) -> u8 {
        self.num() & 0b111
    }
    /// `true` for `xmm8`–`xmm15` (needs a REX `R`/`B` extension bit).
    pub fn is_ext(self) -> bool {
        self.num() >= 8
    }
}

/// The System V SSE argument/return registers, in call order: `xmm0`–`xmm7`.
pub const SSE_ARG_REGS: [Xmm; 8] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
];
