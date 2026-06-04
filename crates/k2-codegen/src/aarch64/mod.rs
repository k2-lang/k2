//! The AArch64 (ARMv8-A) Linux native backend (v0.18, cross-compilation only).
//!
//! This module is the second native target alongside the original x86-64 backend.
//! It contributes:
//!
//! * [`encode`] — a fixed-32-bit-little-endian AArch64 instruction encoder,
//!   byte-exact unit-tested against the ARM ARM (DDI 0487);
//! * [`lower`] — a MIR→AArch64 lowering for the **hello-class** subset (the
//!   AAPCS64 calling convention, the `fp/lr` stack frame, integer/compare/bitwise
//!   arithmetic, the CFG terminators, `print` formatting, the safety-check traps,
//!   and the `write`/`exit` syscalls), driven by the SAME monomorphized MIR the
//!   x86-64 backend compiles.
//!
//! The EM_AARCH64 ELF itself is written by the shared [`crate::elf`] writer
//! (parameterized by `Target::e_machine`), and both backends are stitched by the
//! shared [`crate::link`] pass.
//!
//! ## Scope & honesty
//!
//! The AAPCS64 lowering covers what hello-class programs need; the `*System` heap
//! runtime is **not yet ported** to AArch64, so a reached heap/capability
//! intrinsic returns a clean [`crate::CodegenError::Unsupported`] deferral rather
//! than a miscompile (exactly as the x86 backend rejects out-of-subset
//! constructs). The emitted binaries are **cross-compiled and structurally
//! validated, never executed** in this environment — see [`crate::target`] and
//! `docs/aarch64.md`.

pub(crate) mod encode;
pub(crate) mod link;
pub(crate) mod lower;

#[cfg(test)]
mod tests;
