//! AArch64 program layout: stitch every lowered function into one `.text`
//! segment, emit the AArch64 `_start` shim, concatenate `.rodata`, resolve
//! cross-function relocations, and write the EM_AARCH64 ELF.
//!
//! This mirrors the x86 [`crate::link`] pass but with the AArch64 relocation
//! widths and entry shim:
//!
//! * a cross-function `bl` call is patched as a 26-bit word displacement
//!   ([`patch_branch26`]);
//! * a `.rodata` pointer hole is the 4-instruction `movz`+`movk×3` immediate
//!   sequence, patched lane-by-lane to the string's absolute virtual address
//!   ([`patch_movz_movk`]) — the AArch64 analog of the x86 `mov r64, imm64` hole.
//!
//! The `*System` heap runtime is **not yet ported** to AArch64: a reached
//! function that needs it (a `Runtime`/`State` fixup) is refused with a clean
//! [`CodegenError::Unsupported`] deferral. Hello-class programs emit only
//! `write`/`exit` and produce the clean two-segment ELF with no runtime code.

use std::collections::HashMap;

use k2_mir::{FnId, MirProgram};

use super::encode::{Aarch64Asm, Reg};
use super::lower::Aarch64FnLower;
use crate::elf::{self, ElfImage};
use crate::encode::{Fixup, FixupKind};
use crate::link::find_main;
use crate::target::Target;
use crate::{CodegenError, RoData};

/// One lowered AArch64 function: its code and surviving cross-function fixups.
struct LoweredFn {
    id: FnId,
    code: Vec<u8>,
    fixups: Vec<Fixup>,
    text_off: usize,
}

/// Compiles a whole [`MirProgram`] to a runnable **aarch64** ELF image, or fails
/// with a [`CodegenError`] (the error names the offending construct). The emitted
/// binary is structurally validated but not executed in this environment.
pub(crate) fn compile_program_aarch64(prog: &MirProgram) -> Result<ElfImage, CodegenError> {
    let main_id = find_main(prog).ok_or(CodegenError::NoMain)?;

    // ---- Lower every function. ----
    let mut rodata = RoData::new();
    let mut lowered: Vec<LoweredFn> = Vec::with_capacity(prog.funcs.len());
    for func in &prog.funcs {
        let (code, fixups) = Aarch64FnLower::new(prog, func).lower(&mut rodata)?;
        lowered.push(LoweredFn {
            id: func.id,
            code,
            fixups,
            text_off: 0,
        });
    }

    // ---- Refuse the heap runtime cleanly (not yet ported to aarch64). ----
    if lowered
        .iter()
        .any(|lf| lf.fixups.iter().any(|f| is_runtime_or_state(f.kind)))
    {
        return Err(CodegenError::Unsupported(
            "the *System heap runtime is not yet ported to aarch64; cross-compile \
             hello-class programs or use the x86-64 backend / VM"
                .into(),
        ));
    }

    // ---- Build the AArch64 `_start` shim (the ELF entry). ----
    let start = build_start_shim(main_id);
    let (start_code, start_fixups) = start.finish();

    // ---- Stitch: _start, then every function, recording offsets. ----
    let mut text: Vec<u8> = Vec::new();
    let start_off = 0usize;
    text.extend_from_slice(&start_code);
    let mut fn_offsets: HashMap<FnId, usize> = HashMap::new();
    for lf in &mut lowered {
        lf.text_off = text.len();
        fn_offsets.insert(lf.id, lf.text_off);
        text.extend_from_slice(&lf.code);
    }

    // ---- Compute the rodata virtual address (needed to patch the holes). ----
    let rodata_vaddr = elf::rodata_vaddr_for(text.len());
    // aarch64 hello-class programs need no state segment.
    let state_vaddr = elf::state_vaddr_for(text.len(), rodata.bytes().len());

    // ---- Patch the `_start` shim's fixups, then each function's. ----
    for fx in &start_fixups {
        patch_fixup(
            &mut text,
            start_off,
            fx,
            &fn_offsets,
            rodata_vaddr,
            state_vaddr,
        )?;
    }
    for lf in &lowered {
        for fx in &lf.fixups {
            patch_fixup(
                &mut text,
                lf.text_off,
                fx,
                &fn_offsets,
                rodata_vaddr,
                state_vaddr,
            )?;
        }
    }

    Ok(elf::write_elf_for(
        &text,
        rodata.bytes(),
        0,
        Target::Aarch64Linux.e_machine(),
    ))
}

/// `true` if a fixup targets the (unported) runtime or the state segment.
fn is_runtime_or_state(kind: FixupKind) -> bool {
    matches!(kind, FixupKind::Runtime(_) | FixupKind::State(_))
}

/// Builds the AArch64 `_start` entry shim:
///
/// ```text
///   mov  x0, #0       ; the NULL *System token native main never dereferences
///   bl   main         ; call main (the bl imm26 is patched by the layout pass)
///   ; x0 already holds main's result (the exit code)
///   movz x8, #94      ; SYS_exit_group
///   svc  #0
/// ```
fn build_start_shim(main_id: FnId) -> Aarch64Asm {
    let sysnr = Target::Aarch64Linux.sysnr();
    let mut a = Aarch64Asm::new();
    a.reserve_labels(0);
    a.mov_imm(Reg(0), 0); // x0 = NULL *System
    a.bl_fn(main_id); // call main
    a.mov_imm(Reg(8), sysnr.exit_group); // x8 = exit_group
    a.svc0();
    a
}

/// Patches one surviving fixup at `base + fx.at`.
fn patch_fixup(
    text: &mut [u8],
    base: usize,
    fx: &Fixup,
    fn_offsets: &HashMap<FnId, usize>,
    rodata_vaddr: u64,
    state_vaddr: u64,
) -> Result<(), CodegenError> {
    let site = base + fx.at;
    match fx.kind {
        FixupKind::Call(callee) => {
            let target = *fn_offsets
                .get(&callee)
                .ok_or_else(|| CodegenError::Unsupported("call to an unknown fn".into()))?;
            patch_branch26(text, site, target);
        }
        FixupKind::Data(off) => {
            patch_movz_movk(text, site, rodata_vaddr + off as u64);
        }
        FixupKind::State(off) => {
            patch_movz_movk(text, site, state_vaddr + off as u64);
        }
        FixupKind::Runtime(_) => {
            // Refused up front; should not survive.
            return Err(CodegenError::Unsupported(
                "aarch64 runtime call survived the gate".into(),
            ));
        }
        FixupKind::Local(_) => {
            // Resolved by `Aarch64Asm::finish`; should not survive.
        }
    }
    Ok(())
}

/// Patches a `bl`/`b` instruction at `site` with a 26-bit signed word
/// displacement to `target` (both byte offsets within `.text`).
fn patch_branch26(text: &mut [u8], site: usize, target: usize) {
    let rel_words = (target as i64 - site as i64) / 4;
    let imm26 = (rel_words as u32) & 0x03FF_FFFF;
    let base = u32::from_le_bytes([text[site], text[site + 1], text[site + 2], text[site + 3]]);
    let patched = (base & 0xFC00_0000) | imm26;
    text[site..site + 4].copy_from_slice(&patched.to_le_bytes());
}

/// Patches the 4-instruction `movz`+`movk×3` immediate sequence at `site` (one
/// 16-bit lane per instruction) to the absolute 64-bit `value`. Each instruction
/// carries its `imm16` in bits 5..=20, so we overwrite that field with the
/// corresponding lane while preserving the opcode + `Rd`.
fn patch_movz_movk(text: &mut [u8], site: usize, value: u64) {
    for i in 0..4 {
        let off = site + i * 4;
        let lane = ((value >> (16 * i)) & 0xFFFF) as u32;
        let base = u32::from_le_bytes([text[off], text[off + 1], text[off + 2], text[off + 3]]);
        // Clear the imm16 field (bits 5..=20) and set the lane.
        let patched = (base & !(0xFFFFu32 << 5)) | (lane << 5);
        text[off..off + 4].copy_from_slice(&patched.to_le_bytes());
    }
}
