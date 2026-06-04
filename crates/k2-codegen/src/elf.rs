//! The pure-std ELF64 writer.
//!
//! Produces a **static, non-PIE, directly-executable** x86-64 Linux ELF: an ELF
//! header, one or two program headers, the `.text` machine code, and (when the
//! program prints) a `.rodata` blob of string-literal bytes. There is no dynamic
//! linker, no `PT_INTERP`, no `PT_DYNAMIC`, and no section-header table — none of
//! that is needed for the kernel to map and run the image. The file is written,
//! `chmod +x`-ed, and executed.
//!
//! ## Memory image
//!
//! The load base is the conventional non-PIE address `0x400000`. The first page
//! holds the ELF + program headers and the start of `.text`; the entry point is
//! the very start of `.text` at virtual address `0x401000` (one page in). When
//! the program references string literals, `.rodata` is placed on the next page
//! boundary after the text image, in its own read-only `PT_LOAD`.
//!
//! ```text
//!   file off   vaddr        contents
//!   0x000      0x400000     ELF header (64) + program headers (56 each)
//!   0x1000     0x401000     .text  (entry = _start, then the lowered fns)
//!   0x2000+    0x402000+    .rodata (string bytes), if any
//! ```
//!
//! The kernel requires, per `PT_LOAD`, that `p_vaddr ≡ p_offset (mod p_align)`
//! with `p_align = 0x1000`. We satisfy it trivially: both segments start on a
//! page boundary whose low 12 bits of file offset and virtual address are equal.
//!
//! Because the layout is fixed and known before the bytes are written, the
//! absolute virtual address of every `.rodata` string is computable up-front
//! (`rodata_vaddr + offset`), which is exactly what the lowering's `mov r64,
//! imm64` string-pointer holes are patched with.

/// The non-PIE load base for the whole image.
pub const LOAD_BASE: u64 = 0x40_0000;
/// The page size used for segment alignment (the x86-64 minimum page).
pub const PAGE: u64 = 0x1000;
/// The size in bytes of an ELF64 header.
const EHDR_SIZE: u64 = 64;
/// The size in bytes of one ELF64 program header.
const PHDR_SIZE: u64 = 56;

/// The virtual address where `.text` (and the `_start` entry) begins: one page
/// past the load base, so the headers live in the first mapped page.
pub const TEXT_VADDR: u64 = LOAD_BASE + PAGE;

/// A finished, directly-runnable ELF image plus the virtual addresses the
/// lowering needs to bake into pointer holes.
pub struct ElfImage {
    /// The complete file bytes (write to disk, `chmod +x`, execute).
    pub bytes: Vec<u8>,
    /// The number of `.text` (machine-code) bytes in the image, before page
    /// padding. The peephole size-reduction statistics compare this across a
    /// peephole-on vs peephole-off build of the same program.
    pub text_len: usize,
    /// The virtual address of `.text` / the entry point (`_start`).
    pub text_vaddr: u64,
    /// The virtual address of the `.rodata` blob (meaningful only when the
    /// program has read-only data; equals the text-segment end page otherwise).
    pub rodata_vaddr: u64,
    /// The virtual address of the writable state segment (the allocator registry,
    /// the deterministic clock counter, and the PRNG state), or `0` when the
    /// program needs no state segment.
    pub state_vaddr: u64,
}

/// Rounds `v` up to the next multiple of `align` (a power of two).
fn round_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// Computes the virtual address `.rodata` will load at, given the final `.text`
/// length. This is needed *before* the image is written so the lowering can
/// patch its string-pointer holes; the text length does not change when those
/// holes are patched (they are filled in place), so this is a stable one-shot
/// computation.
///
/// `.text` starts at file offset `PAGE` (vaddr [`TEXT_VADDR`]); `.rodata` is the
/// next page boundary after the end of the text bytes.
pub fn rodata_vaddr_for(text_len: usize) -> u64 {
    let text_end_off = PAGE + text_len as u64;
    let rodata_off = round_up(text_end_off, PAGE);
    LOAD_BASE + rodata_off
}

/// Computes the virtual address the writable **state segment** loads at: the next
/// page boundary after the end of `.rodata`. The state segment is a `.bss`-like
/// `PT_LOAD` (`p_filesz = 0`, zero-mapped by the kernel) holding the allocator
/// registry, the deterministic clock counter, and the PRNG state. Like the rodata
/// address, it is computable up-front from the text + rodata lengths so the
/// runtime routines' absolute `mov r64, imm64` state pointers can be patched.
pub fn state_vaddr_for(text_len: usize, rodata_len: usize) -> u64 {
    let rodata_off = round_up(PAGE + text_len as u64, PAGE);
    let rodata_end_off = rodata_off + rodata_len as u64;
    let state_off = round_up(rodata_end_off, PAGE);
    LOAD_BASE + state_off
}

/// Builds the complete ELF image for the **default x86-64 target**
/// (`e_machine = EM_X86_64`). A thin wrapper over [`write_elf_for`], kept so every
/// existing caller and test compiles and emits byte-identically.
pub fn write_elf(text: &[u8], rodata: &[u8], state_size: u64) -> ElfImage {
    write_elf_for(text, rodata, state_size, 0x3E)
}

/// Builds the complete ELF image from the finalized `.text` machine code, the
/// concatenated `.rodata` bytes, and the writable state segment size (the
/// allocator registry / clock / RNG `.bss`), targeting the given ELF
/// `e_machine`. A single executable `PT_LOAD` is always emitted; a read-only
/// `PT_LOAD` for `.rodata` and a read-write `PT_LOAD` for the zero-mapped state
/// segment are added when nonzero. `e_phnum` is sized accordingly.
///
/// The layout math is endian/architecture-neutral (both x86-64 and aarch64 are
/// LP64 little-endian with a 4 KiB page); only `e_machine` differs between
/// targets. For aarch64 Linux, a 4 KiB page is the configuration this writer
/// targets: `p_align = 0x1000` with `p_vaddr ≡ p_offset (mod p_align)` is
/// satisfied (both segments start on a page boundary), so the kernel maps the
/// static image directly. (aarch64 also supports 16K/64K pages; this writer
/// documents and targets the 4 KiB configuration.)
pub fn write_elf_for(text: &[u8], rodata: &[u8], state_size: u64, e_machine: u16) -> ElfImage {
    let has_rodata = !rodata.is_empty();
    let has_state = state_size > 0;
    let phnum: u16 = 1 + u16::from(has_rodata) + u16::from(has_state);

    // File offsets / virtual addresses. The headers occupy the first page; the
    // text begins at the second page; rodata (if any) at the next page after
    // the text body; the state .bss on the next page after rodata.
    let text_off = PAGE;
    let text_vaddr = TEXT_VADDR;
    let text_end_off = text_off + text.len() as u64;
    let rodata_off = round_up(text_end_off, PAGE);
    let rodata_vaddr = LOAD_BASE + rodata_off;
    let rodata_end_off = rodata_off + rodata.len() as u64;
    let state_off = round_up(rodata_end_off, PAGE);
    let state_vaddr = if has_state { LOAD_BASE + state_off } else { 0 };

    // The text segment maps the file from offset 0 (so the headers are in the
    // image) through the end of the text bytes.
    let text_seg_filesz = text_off + text.len() as u64;

    let mut bytes: Vec<u8> = Vec::new();

    // ---- ELF header (Ehdr, 64 bytes) ----
    bytes.extend_from_slice(&[0x7F, b'E', b'L', b'F']); // e_ident magic
    bytes.push(2); // EI_CLASS  = ELFCLASS64
    bytes.push(1); // EI_DATA   = ELFDATA2LSB (little-endian)
    bytes.push(1); // EI_VERSION
    bytes.push(0); // EI_OSABI  = SYSV
    bytes.push(0); // EI_ABIVERSION
    bytes.extend_from_slice(&[0; 7]); // e_ident padding (to 16 bytes)
    push_u16(&mut bytes, 2); // e_type      = ET_EXEC
    push_u16(&mut bytes, e_machine); // e_machine (EM_X86_64=0x3E / EM_AARCH64=183)
    push_u32(&mut bytes, 1); // e_version
    push_u64(&mut bytes, text_vaddr); // e_entry = _start (start of .text)
    push_u64(&mut bytes, EHDR_SIZE); // e_phoff (phdrs follow the ehdr)
    push_u64(&mut bytes, 0); // e_shoff (no section headers)
    push_u32(&mut bytes, 0); // e_flags
    push_u16(&mut bytes, EHDR_SIZE as u16); // e_ehsize
    push_u16(&mut bytes, PHDR_SIZE as u16); // e_phentsize
    push_u16(&mut bytes, phnum); // e_phnum
    push_u16(&mut bytes, 0); // e_shentsize
    push_u16(&mut bytes, 0); // e_shnum
    push_u16(&mut bytes, 0); // e_shstrndx

    // ---- Program header(s) ----
    // Text segment (R + X), mapping the headers + code from file offset 0.
    push_phdr(
        &mut bytes,
        /* p_type   */ 1, // PT_LOAD
        /* p_flags  */ 5, // PF_R | PF_X
        /* p_offset */ 0,
        /* p_vaddr  */ LOAD_BASE,
        /* p_filesz */ text_seg_filesz,
        /* p_memsz  */ text_seg_filesz,
    );
    if has_rodata {
        // Rodata segment (R only), mapping the string blob on its own page.
        push_phdr(
            &mut bytes,
            /* p_type   */ 1, // PT_LOAD
            /* p_flags  */ 4, // PF_R
            /* p_offset */ rodata_off,
            /* p_vaddr  */ rodata_vaddr,
            /* p_filesz */ rodata.len() as u64,
            /* p_memsz  */ rodata.len() as u64,
        );
    }
    if has_state {
        // State segment (R + W), a zero-mapped `.bss`: `p_filesz = 0`, so the
        // kernel maps `p_memsz` zero-filled bytes with no file bytes added. Holds
        // the allocator registry, the clock counter, and the PRNG state. The
        // `p_offset` is page-aligned and equal mod PAGE to `p_vaddr` (both land on a
        // page boundary), satisfying the kernel's alignment invariant.
        push_phdr(
            &mut bytes,
            /* p_type   */ 1, // PT_LOAD
            /* p_flags  */ 6, // PF_R | PF_W
            /* p_offset */ state_off,
            /* p_vaddr  */ state_vaddr,
            /* p_filesz */ 0,
            /* p_memsz  */ state_size,
        );
    }

    // ---- Pad to the text file offset, then emit .text ----
    debug_assert!(
        bytes.len() as u64 <= text_off,
        "headers overflow the first page"
    );
    bytes.resize(text_off as usize, 0);
    bytes.extend_from_slice(text);

    // ---- Pad to the rodata file offset, then emit .rodata ----
    if has_rodata {
        bytes.resize(rodata_off as usize, 0);
        bytes.extend_from_slice(rodata);
    }

    ElfImage {
        bytes,
        text_len: text.len(),
        text_vaddr,
        rodata_vaddr,
        state_vaddr,
    }
}

/// Appends one `PT_LOAD` program header (56 bytes) with `p_align = PAGE`.
fn push_phdr(
    out: &mut Vec<u8>,
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
) {
    push_u32(out, p_type);
    push_u32(out, p_flags);
    push_u64(out, p_offset);
    push_u64(out, p_vaddr);
    push_u64(out, p_vaddr); // p_paddr == p_vaddr
    push_u64(out, p_filesz);
    push_u64(out, p_memsz);
    push_u64(out, PAGE); // p_align
}

/// Appends a little-endian `u16`.
fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
/// Appends a little-endian `u32`.
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
/// Appends a little-endian `u64`.
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
