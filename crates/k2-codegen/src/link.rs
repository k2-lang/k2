//! Program layout: stitch every lowered function into one `.text` segment,
//! emit the `_start` shim, concatenate `.rodata`, and resolve cross-function
//! relocations.
//!
//! Each [`MirFunction`] is lowered independently (see [`crate::lower`]) to a
//! block of machine code whose intra-function jumps are already patched. What
//! remains is to:
//!
//! 1. Emit a tiny `_start` shim first (the ELF entry), which calls `main` and
//!    `exit()`s with the result.
//! 2. Concatenate every function's code after it, recording each function's byte
//!    offset within `.text`.
//! 3. Patch each surviving `call` relocation (`E8 rel32`) to the callee's offset
//!    and each `.rodata` pointer hole (`mov r64, imm64`) to the string's absolute
//!    virtual address.
//! 4. Hand the finished `.text` and `.rodata` to the ELF writer.
//!
//! Because the ELF is non-PIE with a fixed load base, the `.rodata` virtual
//! address is computable from the final `.text` length *before* the image is
//! written; that address is what the `mov r64, imm64` string holes are patched
//! with.

use std::collections::HashMap;

use k2_mir::{FnId, MirProgram};

use crate::elf::{self, ElfImage};
use crate::encode::{Asm, FixupKind};
use crate::lower::FnLower;
use crate::reg::Gpr;
use crate::{CodegenError, RoData};

/// One lowered function's code plus its unresolved cross-function fixups.
struct LoweredFn {
    /// The function's id.
    id: FnId,
    /// The finalized machine code (intra-function jumps already patched).
    code: Vec<u8>,
    /// Surviving `Call`/`Data` fixups, with `at` offsets relative to `code[0]`.
    fixups: Vec<crate::encode::Fixup>,
    /// The byte offset of this function within the assembled `.text` (filled in
    /// during stitching).
    text_off: usize,
}

/// Compiles a whole [`MirProgram`] to a runnable ELF image, or fails with a
/// [`CodegenError`] if any reached function is outside the v0.14 subset (the
/// error names the offending construct).
pub fn compile_program(prog: &MirProgram) -> Result<ElfImage, CodegenError> {
    let main_id = find_main(prog).ok_or(CodegenError::NoMain)?;

    // ---- Lower every function, collecting code + fixups + the rodata blob. ----
    let mut rodata = RoData::new();
    let mut lowered: Vec<LoweredFn> = Vec::with_capacity(prog.funcs.len());
    for func in &prog.funcs {
        let (code, fixups) = FnLower::new(prog, func).lower(&mut rodata)?;
        lowered.push(LoweredFn {
            id: func.id,
            code,
            fixups,
            text_off: 0,
        });
    }

    // ---- Build the `_start` shim (the ELF entry). ----
    // It runs first in `.text`, so its offset is 0. It calls `main` (a cross-fn
    // `Call` fixup to the real entry id) and exits with main's RAX result.
    let start = build_start_shim(main_id);
    let (start_code, start_fixups) = start.finish();

    // ---- Stitch: _start, then every function, recording offsets. ----
    let mut text: Vec<u8> = Vec::new();
    // The shim is first.
    let start_off = 0usize;
    text.extend_from_slice(&start_code);
    // Each function follows; record its offset.
    let mut fn_offsets: HashMap<FnId, usize> = HashMap::new();
    for lf in &mut lowered {
        lf.text_off = text.len();
        fn_offsets.insert(lf.id, lf.text_off);
        text.extend_from_slice(&lf.code);
    }

    // ---- Compute the rodata virtual address (needed to patch Data holes). ----
    let rodata_vaddr = elf::rodata_vaddr_for(text.len());

    // ---- Patch the `_start` shim's `call main` fixup. ----
    for fx in &start_fixups {
        match fx.kind {
            FixupKind::Call(callee) => {
                let target = *fn_offsets
                    .get(&callee)
                    .ok_or_else(|| CodegenError::Unsupported("call to an unknown fn".into()))?;
                patch_rel32(&mut text, start_off + fx.at, target);
            }
            FixupKind::Data(off) => {
                patch_abs64(&mut text, start_off + fx.at, rodata_vaddr + off as u64);
            }
            FixupKind::Local(_) => {
                // Already resolved by `Asm::finish`; should not survive.
            }
        }
    }

    // ---- Patch each function's surviving fixups. ----
    for lf in &lowered {
        for fx in &lf.fixups {
            let site = lf.text_off + fx.at;
            match fx.kind {
                FixupKind::Call(callee) => {
                    let target = *fn_offsets
                        .get(&callee)
                        .ok_or_else(|| CodegenError::Unsupported("call to an unknown fn".into()))?;
                    patch_rel32(&mut text, site, target);
                }
                FixupKind::Data(off) => {
                    patch_abs64(&mut text, site, rodata_vaddr + off as u64);
                }
                FixupKind::Local(_) => {}
            }
        }
    }

    Ok(elf::write_elf(&text, rodata.bytes()))
}

/// Builds the `_start` entry shim. It clears RDI (the `*System` token native
/// `main` never dereferences), calls `main` via a cross-function `Call` fixup to
/// `main_id`, then maps main's RAX result to `exit(rax)`. `main`'s lowering
/// already places the correct exit code in RAX at its `Return` (0 on success,
/// 1 on an escaped error); for a `main` that returns an integer value that value
/// flows straight through — so the process exit code is exactly main's result.
fn build_start_shim(main_id: FnId) -> Asm {
    let mut a = Asm::new();
    a.reserve_labels(0);
    // xor rdi, rdi  ->  RDI = 0 (the NULL *System handle).
    a.xor_rr(Gpr::Rdi, Gpr::Rdi);
    // call main (the rel32 is patched by the layout pass).
    a.call_fn(main_id);
    // mov rdi, rax  (exit code = main's result).
    a.mov_rr(Gpr::Rdi, Gpr::Rax);
    // mov rax, 60   (SYS_exit).
    a.mov_ri(Gpr::Rax, 60);
    a.syscall();
    a
}

/// Locates the entry `main` function id (by name, falling back to the first
/// declared entry), matching the VM's `find_main`.
fn find_main(prog: &MirProgram) -> Option<FnId> {
    if let Some(f) = prog.funcs.iter().find(|f| f.name == "main") {
        return Some(f.id);
    }
    prog.entries.first().copied()
}

/// Writes a `rel32` displacement at `text[site..site+4]` for a near call/jump:
/// `target - (site + 4)`.
fn patch_rel32(text: &mut [u8], site: usize, target: usize) {
    let rel = (target as i64) - (site as i64 + 4);
    let rel32 = rel as i32;
    text[site..site + 4].copy_from_slice(&rel32.to_le_bytes());
}

/// Writes an absolute 64-bit value at `text[site..site+8]` (a `mov r64, imm64`
/// `.rodata` pointer hole).
fn patch_abs64(text: &mut [u8], site: usize, value: u64) {
    text[site..site + 8].copy_from_slice(&value.to_le_bytes());
}
