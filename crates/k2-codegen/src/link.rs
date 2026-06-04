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

use crate::aarch64;
use crate::elf::{self, ElfImage};
use crate::encode::{Asm, FixupKind};
use crate::lower::FnLower;
use crate::reg::Gpr;
use crate::runtime::{self, RuntimeFn};
use crate::target::Target;
use crate::{CodegenError, RoData};

/// The fixed seed for the deterministic splitmix64 PRNG, matching the VM
/// (`Vm::new`'s `rng: 0x9E37_79B9_7F4A_7C15`). `_start` writes it into the state
/// segment so `sys.random` is reproducible native == VM.
const RNG_SEED: i64 = 0x9E37_79B9_7F4A_7C15u64 as i64;

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

/// Compiles a whole [`MirProgram`] to a runnable ELF image for `target`, or fails
/// with a [`CodegenError`] if any reached function is outside the selected
/// target's subset (the error names the offending construct).
///
/// The x86-64 path is the original pipeline, byte-for-byte. The aarch64 path
/// (cross-compilation only) uses the [`crate::aarch64`] encoder + lowering and is
/// dispatched separately because its `_start` shim and relocation widths differ.
pub fn compile_program(prog: &MirProgram, target: Target) -> Result<ElfImage, CodegenError> {
    match target {
        Target::X86_64Linux => compile_program_x86(prog),
        Target::Aarch64Linux => aarch64::link::compile_program_aarch64(prog),
    }
}

/// Compiles a whole [`MirProgram`] to a runnable x86-64 ELF image (the original
/// pipeline, unchanged).
fn compile_program_x86(prog: &MirProgram) -> Result<ElfImage, CodegenError> {
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

    // ---- Decide whether this program needs the `*System` runtime. ----
    // The runtime support routines + the writable state segment are emitted only
    // when some reached function uses a heap / capability intrinsic (so hello.k2
    // and the pre-v0.16 corpus keep their exact two-segment image). The lowering
    // signals this by having emitted a `Runtime`/`State` fixup anywhere.
    let needs_runtime = lowered
        .iter()
        .any(|lf| lf.fixups.iter().any(|f| is_runtime_or_state(f.kind)));

    // ---- Build the `_start` shim (the ELF entry). ----
    // It runs first in `.text`, so its offset is 0. When the program uses the
    // runtime, the shim first seeds the deterministic PRNG and initializes the
    // default-allocator registry slot; then it calls `main` and exits with its
    // RAX result.
    let start = build_start_shim(main_id, needs_runtime);
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

    // ---- Append the runtime support routines (when needed), recording each
    //      routine's `.text` offset for the `Runtime` fixups. ----
    let mut runtime_offsets: HashMap<RuntimeFn, usize> = HashMap::new();
    let mut runtime_fixups: Vec<(usize, crate::encode::Fixup)> = Vec::new();
    if needs_runtime {
        for rt in RuntimeFn::ALL {
            let (code, fixups) = runtime::emit(rt).finish();
            let off = text.len();
            runtime_offsets.insert(rt, off);
            for fx in fixups {
                runtime_fixups.push((off, fx));
            }
            text.extend_from_slice(&code);
        }
    }

    // ---- Compute the rodata + state virtual addresses (needed to patch holes). ----
    let rodata_vaddr = elf::rodata_vaddr_for(text.len());
    let state_size = if needs_runtime {
        runtime::state_segment_size()
    } else {
        0
    };
    let state_vaddr = elf::state_vaddr_for(text.len(), rodata.bytes().len());

    // ---- Patch the `_start` shim's fixups. ----
    for fx in &start_fixups {
        patch_fixup(
            &mut text,
            start_off,
            fx,
            &fn_offsets,
            &runtime_offsets,
            rodata_vaddr,
            state_vaddr,
        )?;
    }

    // ---- Patch each function's surviving fixups. ----
    for lf in &lowered {
        for fx in &lf.fixups {
            patch_fixup(
                &mut text,
                lf.text_off,
                fx,
                &fn_offsets,
                &runtime_offsets,
                rodata_vaddr,
                state_vaddr,
            )?;
        }
    }

    // ---- Patch the runtime routines' own (State) fixups. ----
    for (base, fx) in &runtime_fixups {
        patch_fixup(
            &mut text,
            *base,
            fx,
            &fn_offsets,
            &runtime_offsets,
            rodata_vaddr,
            state_vaddr,
        )?;
    }

    Ok(elf::write_elf(&text, rodata.bytes(), state_size))
}

/// `true` if a fixup targets the runtime prelude or the writable state segment
/// (the signal that this program needs the `*System` runtime emitted).
fn is_runtime_or_state(kind: FixupKind) -> bool {
    matches!(kind, FixupKind::Runtime(_) | FixupKind::State(_))
}

/// Patches one surviving fixup at `base + fx.at` against the resolved fn / runtime
/// offsets and the rodata / state base addresses.
#[allow(clippy::too_many_arguments)]
fn patch_fixup(
    text: &mut [u8],
    base: usize,
    fx: &crate::encode::Fixup,
    fn_offsets: &HashMap<FnId, usize>,
    runtime_offsets: &HashMap<RuntimeFn, usize>,
    rodata_vaddr: u64,
    state_vaddr: u64,
) -> Result<(), CodegenError> {
    let site = base + fx.at;
    match fx.kind {
        FixupKind::Call(callee) => {
            let target = *fn_offsets
                .get(&callee)
                .ok_or_else(|| CodegenError::Unsupported("call to an unknown fn".into()))?;
            patch_rel32(text, site, target);
        }
        FixupKind::Runtime(rt) => {
            let target = *runtime_offsets.get(&rt).ok_or_else(|| {
                CodegenError::Unsupported("call to an unemitted runtime fn".into())
            })?;
            patch_rel32(text, site, target);
        }
        FixupKind::Data(off) => {
            patch_abs64(text, site, rodata_vaddr + off as u64);
        }
        FixupKind::State(off) => {
            patch_abs64(text, site, state_vaddr + off as u64);
        }
        FixupKind::Local(_) => {
            // Already resolved by `Asm::finish`; should not survive.
        }
    }
    Ok(())
}

/// Builds the `_start` entry shim. It clears RDI (the `*System` token native
/// `main` never dereferences), calls `main` via a cross-function `Call` fixup to
/// `main_id`, then maps main's RAX result to `exit(rax)`. `main`'s lowering
/// already places the correct exit code in RAX at its `Return` (0 on success,
/// 1 on an escaped error); for a `main` that returns an integer value that value
/// flows straight through — so the process exit code is exactly main's result.
fn build_start_shim(main_id: FnId, needs_runtime: bool) -> Asm {
    let mut a = Asm::new();
    a.reserve_labels(0);
    if needs_runtime {
        // Seed the PRNG state: rng_state = RNG_SEED.
        a.mov_ri_state(Gpr::R11, runtime::ST_RNG_STATE);
        a.mov_ri(Gpr::Rax, RNG_SEED);
        a.mov_store_mem(Gpr::R11, 0, Gpr::Rax);
        // reg_next = 1 (slot 0 is the always-present default allocator; its kind
        // tag is 0 = Default, already zero-mapped, and clock_nanos starts at 0).
        a.mov_ri_state(Gpr::R11, runtime::ST_REG_NEXT);
        a.mov_ri(Gpr::Rax, 1);
        a.mov_store_mem(Gpr::R11, 0, Gpr::Rax);
    }
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
/// declared entry), matching the VM's `find_main`. Shared with the aarch64 link
/// pass.
pub(crate) fn find_main(prog: &MirProgram) -> Option<FnId> {
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
