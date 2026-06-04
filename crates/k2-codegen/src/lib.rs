//! k2 v0.15 — the pure-std x86-64 native backend (near-full coverage).
//!
//! This crate is k2's native code path: it turns a *subset* of a monomorphized
//! [`k2_mir::MirProgram`] into a real, static, directly-runnable x86-64 Linux ELF
//! — with **no** Cranelift, **no** LLVM, **no** libc, and **no** external crates
//! of any kind. Everything is hand-rolled against `std`:
//!
//! * [`encode`] — an x86-64 instruction encoder that emits the exact machine-code
//!   bytes (integer ALU/`idiv`/shifts; `cmp`/`setcc`/`jcc`; `push`/`pop`/`lea`;
//!   sized loads/stores + `movzx`/`movsx` through an arbitrary base register;
//!   `rep movsb`; and the SSE2 scalar-double family `movsd`/`addsd`/.../`ucomisd`/
//!   `cvtsi2sd`/`cvttsd2si`). Every method is byte-level unit-tested.
//! * [`elf`] — an ELF64 writer that produces a static non-PIE `ET_EXEC` with one
//!   or two `PT_LOAD` segments; the file is `chmod +x`-ed and runs with no linker.
//! * [`layout`] — a faithful port of the `reflect.rs` byte-layout oracle, so
//!   aggregate layouts agree with `@sizeOf`/`@offsetOf`-derived constants.
//! * [`frame`] / [`regalloc`] — a per-function stack-frame planner and a
//!   linear-scan register allocator (callee-saved pool, spill to the frame).
//! * [`lower`] / [`fmt_native`] — the MIR -> machine-code lowering: place
//!   projections (`Field`/`Index`/`Deref`/`SliceMeta`/`Payload`, load & store),
//!   aggregate construction (`Aggregate`/`Ref`/`MakeSlice`/optional & error-union
//!   constructors), runtime `print` formatting matching `k2_vm::fmt`, the full
//!   System V ABI (int + SSE args, `>6` stack args, small aggregates in RAX:RDX,
//!   MEMORY-class via a hidden `sret` pointer), f64 arithmetic/compare/casts, and
//!   the `write`/`exit` syscalls, plus the `_start` shim.
//!
//! The portable encoder/ELF/lowering/layout logic builds on **every** host; the
//! crate's *execution* tests are gated `#[cfg(all(target_arch = "x86_64",
//! target_os = "linux"))]` so they run on CI but never break other platforms.
//!
//! ## Scope (v0.16)
//!
//! Accepted: scalar integers / `bool` / pointers / `f64`, structs / fixed arrays
//! / slices / optionals / error unions as stack values, all place projections,
//! aggregate construction + `Ref` + `MakeSlice`, runtime-formatted `print`
//! (`{d}`/`{s}`/`{c}`/`{x}`/`{X}`/`{b}`/`{o}`/`{}` with width/alignment padding),
//! recursion, `>6`-arg and aggregate-by-value calls, enum `switch`, the
//! `@no_*_overflow`/`narrow_fits` safety predicates, an escaped-error exit path,
//! and — new in v0.16 — the **`*System` runtime** via raw Linux syscalls
//! ([`runtime`]): an `mmap`-backed heap (`create`/`alloc`/`free`/`realloc`/
//! `destroy`), the handle-based allocator registry with GPA leak + double-free
//! detection (clean exit 134, matching the VM), the deterministic clock and
//! splitmix64 PRNG, and offline-absent `env`. Still **out** — rejected up-front
//! with a [`CodegenError::Unsupported`] message rather than miscompiled, with the
//! VM path via `k2c run` available: the concurrency scheduler, the `*Build`
//! capability, an *un-monomorphized generic heap container* (`ArrayList`/`List`,
//! whose shared `deferred`-element methods cannot agree on an element stride with
//! a concrete reader — see [`lower`]), and runtime (non-constant) f64 formatting.
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
mod fmt_native;
mod frame;
mod layout;
mod link;
mod lower;
mod mir_ids;
mod reg;
mod regalloc;
mod runtime;

#[cfg(test)]
mod tests;

pub use elf::ElfImage;

use k2_mir::MirProgram;

/// A reason native code generation could not proceed for a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    /// The program uses a construct outside the v0.16 native subset (the
    /// concurrency scheduler, the `*Build` capability, an un-monomorphized generic
    /// heap container, runtime f64 formatting). The string names the offending
    /// feature so the driver can print an actionable message and suggest `k2c run`.
    Unsupported(String),
    /// The program has no `main` entry point to compile.
    NoMain,
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodegenError::Unsupported(msg) => {
                write!(f, "unsupported by the v0.16 native backend: {msg}")
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
    link::compile_program(prog)
}
