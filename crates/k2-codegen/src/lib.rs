//! k2 v0.14 — the pure-std x86-64 native backend foundation.
//!
//! This crate is the FOUNDATION of k2's native code path: it turns a *subset* of
//! a monomorphized [`k2_mir::MirProgram`] into a real, static, directly-runnable
//! x86-64 Linux ELF — with **no** Cranelift, **no** LLVM, **no** libc, and **no**
//! external crates of any kind. Everything is hand-rolled against `std`:
//!
//! * [`encode`] — an x86-64 instruction encoder that emits the exact machine-code
//!   bytes for the instructions the subset needs (`mov`/`add`/`sub`/`imul`/`idiv`,
//!   `cmp`, conditional + unconditional jumps, `push`/`pop`, `lea`, `call`/`ret`,
//!   `syscall`, and the `[rbp - N]` stack-slot + immediate addressing modes).
//!   Every method is unit-testable down to the byte.
//! * [`elf`] — an ELF64 writer that produces a static non-PIE `ET_EXEC` with one
//!   or two `PT_LOAD` segments and an entry point; the file is `chmod +x`-ed and
//!   runs with no dynamic linker.
//! * [`lower`] / [`layout`] — the MIR -> machine-code lowering for the subset
//!   (integer locals in stack slots; width-correct arithmetic / compare / bitwise
//!   / shift; `Goto`/`Branch`/`Switch`/`Return`/`Trap`/`Unreachable`; SysV direct
//!   calls; and the `write`/`exit` syscall intrinsics so a program can produce
//!   observable output), plus the `_start` shim that runs `main` and `exit()`s.
//!
//! The portable encoder/ELF/lowering logic builds on **every** host; the crate's
//! *execution* tests are gated `#[cfg(all(target_arch = "x86_64", target_os =
//! "linux"))]` so they run on CI but never break other platforms.
//!
//! ## Scope (v0.14)
//!
//! Accepted: scalar integer / `bool` locals (and an opaque `*System` handle
//! threaded through `main`), integer `Binary`/`Unary`/`Cast`, direct `Call`s, the
//! stdout/stderr writer + fixed-string `print` intrinsics, the `@no_*_overflow` /
//! `narrow_fits` safety predicates that guard a `Trap`, and the
//! `Goto`/`Branch`/`Switch`/`Return`/`Trap`/`Unreachable` terminators. Anything
//! else (floats, slices/arrays/structs as values, projected places, `Ref`,
//! aggregate construction, runtime-formatted `print`, register allocation, jump
//! tables, additional syscalls) is **out** — it is rejected up-front with a
//! [`CodegenError::Unsupported`] message rather than miscompiled, and the VM path
//! via `k2c run` remains available. Full language coverage is v0.15.
//!
//! ## Public API
//!
//! [`compile_program_to_elf`] runs the whole pipeline:
//!
//! ```ignore
//! let img = k2_codegen::compile_program_to_elf(&prog)?;
//! std::fs::write("a.out", &img.bytes)?;
//! // chmod +x a.out && ./a.out
//! ```

mod elf;
mod encode;
mod layout;
mod lower;
mod mir_ids;
mod reg;

#[cfg(test)]
mod tests;

pub use elf::ElfImage;

use k2_mir::MirProgram;

/// A reason native code generation could not proceed for a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    /// The program uses a construct outside the v0.14 native subset. The string
    /// names the offending feature (e.g. `"rvalue Aggregate in `main`"`) so the
    /// driver can print an actionable message and suggest `k2c run`.
    Unsupported(String),
    /// The program has no `main` entry point to compile.
    NoMain,
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodegenError::Unsupported(msg) => {
                write!(f, "unsupported by the v0.14 native backend: {msg}")
            }
            CodegenError::NoMain => write!(f, "program has no `main` entry point"),
        }
    }
}

impl std::error::Error for CodegenError {}

/// The accumulating `.rodata` blob: every string literal a recognized `print`
/// emits, plus the fixed panic messages, concatenated. [`RoData::intern`]
/// appends a byte string and returns its offset, which a `mov r64, imm64` hole
/// later resolves to `rodata_vaddr + offset`. Identical strings are deduplicated
/// (an optional correctness-neutral size win).
pub(crate) struct RoData {
    /// The concatenated bytes of every interned string.
    bytes: Vec<u8>,
    /// Dedup map: byte-string -> its offset in `bytes`.
    seen: std::collections::HashMap<Vec<u8>, u32>,
}

impl RoData {
    /// A fresh, empty rodata accumulator.
    fn new() -> RoData {
        RoData {
            bytes: Vec::new(),
            seen: std::collections::HashMap::new(),
        }
    }

    /// Appends `s` (deduplicated) and returns its byte offset within `.rodata`.
    fn intern(&mut self, s: &[u8]) -> u32 {
        if let Some(&off) = self.seen.get(s) {
            return off;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s);
        self.seen.insert(s.to_vec(), off);
        off
    }

    /// The concatenated `.rodata` bytes.
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Compiles a monomorphized [`MirProgram`] to a static, directly-executable
/// x86-64 Linux ELF, or fails with a [`CodegenError`] naming the first construct
/// outside the v0.14 subset (the gate runs before any byte is emitted, so a
/// non-subset program fails cleanly — it is never miscompiled).
///
/// The result's [`ElfImage::bytes`] are a complete file: write them to disk,
/// `chmod +x`, and run. This function never panics the host process on a
/// subset-valid program — out-of-subset constructs become an `Err`, not a panic.
pub fn compile_program_to_elf(prog: &MirProgram) -> Result<ElfImage, CodegenError> {
    layout::compile_program(prog)
}
