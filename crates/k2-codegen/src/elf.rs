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
/// The size in bytes of one ELF64 section header.
const SHDR_SIZE: u64 = 64;

// ELF section types used by the section-header table (the executable's
// debug-info path only needs PROGBITS / STRTAB / NULL).
/// `SHT_NULL`: the mandatory index-0 inactive section.
const SHT_NULL: u32 = 0;
/// `SHT_PROGBITS`: program-defined bytes (`.text`/`.rodata`/`.debug_*`).
const SHT_PROGBITS: u32 = 1;
/// `SHT_STRTAB`: a string table (`.shstrtab`).
const SHT_STRTAB: u32 = 3;

// ELF section flags. The `.debug_*` sections carry **none** of these (so the
// kernel never maps them and they stay outside every `PT_LOAD`); only `.text`
// and `.rodata` are `SHF_ALLOC`.
/// `SHF_ALLOC`: the section occupies memory at run time.
const SHF_ALLOC: u64 = 0x2;
/// `SHF_EXECINSTR`: the section holds executable instructions.
const SHF_EXECINSTR: u64 = 0x4;

/// The four hand-emitted DWARF debug section blobs (DWARF v5), produced by
/// [`crate::dwarf::build`] and handed to [`write_elf_with_debug`]. Each is a
/// finished, self-contained byte buffer; the ELF writer only places them into
/// non-loaded `SHT_PROGBITS` sections and records their offsets/sizes in the
/// section-header table. They never participate in a `PT_LOAD`, so they cannot
/// change the executed image.
pub struct DebugSections {
    /// `.debug_info`: the compilation-unit DIE + per-function subprogram DIEs.
    pub info: Vec<u8>,
    /// `.debug_abbrev`: the abbreviation table the DIEs reference.
    pub abbrev: Vec<u8>,
    /// `.debug_line`: the line-number program mapping code addresses to
    /// `(file, line)`.
    pub line: Vec<u8>,
    /// `.debug_str`: the string pool the DIEs reference via `DW_FORM_strp`.
    pub str_: Vec<u8>,
}

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
    write_elf_with_debug(text, rodata, state_size, e_machine, None)
}

/// Builds the complete ELF image, optionally appending a **section-header table**
/// plus the four DWARF `.debug_*` sections after the loaded image.
///
/// When `debug` is `None`, the output is **byte-for-byte identical** to the
/// historical [`write_elf_for`] (`e_shoff`/`e_shnum`/`e_shentsize`/`e_shstrndx`
/// stay zero, nothing follows the loaded segments) — so every existing caller and
/// the whole golden test corpus are unaffected, and the `-g`-off `k2c build-native`
/// keeps emitting the exact v0.26 bytes.
///
/// When `debug` is `Some`, the loaded image (Ehdr + Phdrs + `.text` + `.rodata`,
/// and the program headers that describe them) is emitted **unchanged**; only the
/// four section-table-related Ehdr fields are filled in, and the debug sections,
/// the section-name string table, and the section-header table are appended as
/// trailing, **unmapped** `SHT_PROGBITS`/`SHT_STRTAB` metadata. The kernel maps
/// and runs the identical image whether or not `debug` is set, which is the
/// milestone's non-negotiable "DWARF never changes the executed code" guarantee.
///
/// Section-header table layout (executable, `debug = Some`):
///
/// ```text
///   [0] (null)         SHT_NULL
///   [1] .text          SHT_PROGBITS  ALLOC|EXECINSTR  sh_addr = TEXT_VADDR
///   [2] .rodata        SHT_PROGBITS  ALLOC            (only if rodata present)
///   [3] .debug_abbrev  SHT_PROGBITS  (no flags, sh_addr = 0)
///   [4] .debug_str     SHT_PROGBITS
///   [5] .debug_line    SHT_PROGBITS
///   [6] .debug_info    SHT_PROGBITS
///   [7] .shstrtab      SHT_STRTAB
/// ```
pub fn write_elf_with_debug(
    text: &[u8],
    rodata: &[u8],
    state_size: u64,
    e_machine: u16,
    debug: Option<&DebugSections>,
) -> ElfImage {
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

    // ---- Pre-compute the trailing debug-section + section-header layout. ----
    // This must happen *before* the Ehdr is written so its `e_shoff`/`e_shnum`/
    // `e_shstrndx` fields are correct, but it changes nothing in the loaded image:
    // every offset below is past the last loaded byte. `None` => the section table
    // is absent and these stay zero (the historical byte-identical path).
    let loaded_end_off = if has_rodata {
        rodata_off + rodata.len() as u64
    } else {
        text_seg_filesz
    };
    let layout = debug.map(|ds| SectionLayout::compute(ds, loaded_end_off, has_rodata));
    let (e_shoff, e_shnum, e_shstrndx) = match &layout {
        Some(l) => (l.shoff, l.shnum, l.shstrndx),
        None => (0, 0, 0),
    };
    let e_shentsize: u16 = if layout.is_some() {
        SHDR_SIZE as u16
    } else {
        0
    };

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
    push_u64(&mut bytes, e_shoff); // e_shoff (0 unless debug sections present)
    push_u32(&mut bytes, 0); // e_flags
    push_u16(&mut bytes, EHDR_SIZE as u16); // e_ehsize
    push_u16(&mut bytes, PHDR_SIZE as u16); // e_phentsize
    push_u16(&mut bytes, phnum); // e_phnum
    push_u16(&mut bytes, e_shentsize); // e_shentsize
    push_u16(&mut bytes, e_shnum); // e_shnum
    push_u16(&mut bytes, e_shstrndx); // e_shstrndx

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

    // ---- (debug only) Append the .debug_* sections + the section-header table. ----
    // Everything below is past `loaded_end_off`, i.e. outside every PT_LOAD, so the
    // mapped/executed image is byte-identical to the `debug = None` path.
    if let (Some(ds), Some(l)) = (debug, &layout) {
        debug_assert_eq!(bytes.len() as u64, loaded_end_off);
        pad_to(&mut bytes, l.abbrev_off);
        bytes.extend_from_slice(&ds.abbrev);
        pad_to(&mut bytes, l.str_off);
        bytes.extend_from_slice(&ds.str_);
        pad_to(&mut bytes, l.line_off);
        bytes.extend_from_slice(&ds.line);
        pad_to(&mut bytes, l.info_off);
        bytes.extend_from_slice(&ds.info);
        pad_to(&mut bytes, l.shstrtab_off);
        bytes.extend_from_slice(&l.shstrtab);
        pad_to(&mut bytes, l.shoff);

        // [0] null section.
        push_shdr(&mut bytes, 0, SHT_NULL, 0, 0, 0, 16, 0);
        // [1] .text (PROGBITS, ALLOC|EXEC) — real sh_addr so addr2line/objdump map it.
        push_shdr(
            &mut bytes,
            l.name_text,
            SHT_PROGBITS,
            SHF_ALLOC | SHF_EXECINSTR,
            text_vaddr,
            text_off,
            text.len() as u64,
            16,
        );
        // [2] .rodata (PROGBITS, ALLOC), only when the program has read-only data.
        if has_rodata {
            push_shdr(
                &mut bytes,
                l.name_rodata,
                SHT_PROGBITS,
                SHF_ALLOC,
                rodata_vaddr,
                rodata_off,
                rodata.len() as u64,
                16,
            );
        }
        // [3..7] the four DWARF sections: SHT_PROGBITS, no flags, sh_addr = 0 (so
        // they never land in a PT_LOAD), align 1.
        push_shdr(
            &mut bytes,
            l.name_abbrev,
            SHT_PROGBITS,
            0,
            0,
            l.abbrev_off,
            ds.abbrev.len() as u64,
            1,
        );
        push_shdr(
            &mut bytes,
            l.name_str,
            SHT_PROGBITS,
            0,
            0,
            l.str_off,
            ds.str_.len() as u64,
            1,
        );
        push_shdr(
            &mut bytes,
            l.name_line,
            SHT_PROGBITS,
            0,
            0,
            l.line_off,
            ds.line.len() as u64,
            1,
        );
        push_shdr(
            &mut bytes,
            l.name_info,
            SHT_PROGBITS,
            0,
            0,
            l.info_off,
            ds.info.len() as u64,
            1,
        );
        // [last] .shstrtab (STRTAB).
        push_shdr(
            &mut bytes,
            l.name_shstrtab,
            SHT_STRTAB,
            0,
            0,
            l.shstrtab_off,
            l.shstrtab.len() as u64,
            1,
        );
    }

    ElfImage {
        bytes,
        text_len: text.len(),
        text_vaddr,
        rodata_vaddr,
        state_vaddr,
    }
}

/// The pre-computed file offsets, the section-name string table, and the derived
/// `e_shoff`/`e_shnum`/`e_shstrndx` for the trailing section-header table. Built
/// once (before the Ehdr is written) so the Ehdr fields are correct, then consumed
/// when the sections are appended. All offsets are past the loaded image.
struct SectionLayout {
    /// File offset of each appended blob (8-aligned for tidiness; align 1 is legal).
    abbrev_off: u64,
    str_off: u64,
    line_off: u64,
    info_off: u64,
    shstrtab_off: u64,
    /// File offset of the section-header table itself (`e_shoff`).
    shoff: u64,
    /// `e_shnum`: the number of section headers (7 without rodata, 8 with).
    shnum: u16,
    /// `e_shstrndx`: the `.shstrtab` index (computed dynamically — rodata shifts it).
    shstrndx: u16,
    /// The `.shstrtab` bytes (a `\0`-led pool of the section names).
    shstrtab: Vec<u8>,
    /// `sh_name` offsets into `.shstrtab` for each named section.
    name_text: u32,
    name_rodata: u32,
    name_abbrev: u32,
    name_str: u32,
    name_line: u32,
    name_info: u32,
    name_shstrtab: u32,
}

impl SectionLayout {
    /// Lays out the trailing sections starting at `loaded_end` (the first byte past
    /// the last `PT_LOAD` content), building the `.shstrtab` and computing every
    /// file offset + the three Ehdr section-table fields.
    fn compute(ds: &DebugSections, loaded_end: u64, has_rodata: bool) -> SectionLayout {
        // Build the section-name string table.
        let mut shstrtab: Vec<u8> = vec![0];
        let name_text = add_shstr(&mut shstrtab, ".text");
        let name_rodata = add_shstr(&mut shstrtab, ".rodata");
        let name_abbrev = add_shstr(&mut shstrtab, ".debug_abbrev");
        let name_str = add_shstr(&mut shstrtab, ".debug_str");
        let name_line = add_shstr(&mut shstrtab, ".debug_line");
        let name_info = add_shstr(&mut shstrtab, ".debug_info");
        let name_shstrtab = add_shstr(&mut shstrtab, ".shstrtab");

        // File offsets for the appended blobs (8-aligned starts).
        let abbrev_off = round_up(loaded_end, 8);
        let str_off = round_up(abbrev_off + ds.abbrev.len() as u64, 8);
        let line_off = round_up(str_off + ds.str_.len() as u64, 8);
        let info_off = round_up(line_off + ds.line.len() as u64, 8);
        let shstrtab_off = round_up(info_off + ds.info.len() as u64, 8);
        let shoff = round_up(shstrtab_off + shstrtab.len() as u64, 8);

        // Section count + the `.shstrtab` index: 1 null + .text (+ .rodata) + 4
        // debug + .shstrtab. The `.shstrtab` is always last.
        let shnum: u16 = if has_rodata { 8 } else { 7 };
        let shstrndx = shnum - 1;

        SectionLayout {
            abbrev_off,
            str_off,
            line_off,
            info_off,
            shstrtab_off,
            shoff,
            shnum,
            shstrndx,
            shstrtab,
            name_text,
            name_rodata,
            name_abbrev,
            name_str,
            name_line,
            name_info,
            name_shstrtab,
        }
    }
}

/// Appends a NUL-terminated section name to `.shstrtab` and returns its offset.
fn add_shstr(out: &mut Vec<u8>, name: &str) -> u32 {
    let off = out.len() as u32;
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    off
}

/// Zero-pads `out` up to file offset `to` (a no-op if already at/past it).
fn pad_to(out: &mut Vec<u8>, to: u64) {
    if (out.len() as u64) < to {
        out.resize(to as usize, 0);
    }
}

/// Appends one 64-byte `Elf64_Shdr` (`sh_link`/`sh_info`/`sh_entsize` zero — none
/// of the executable's sections need them).
#[allow(clippy::too_many_arguments)]
fn push_shdr(
    out: &mut Vec<u8>,
    name: u32,
    typ: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    addralign: u64,
) {
    push_u32(out, name); // sh_name
    push_u32(out, typ); // sh_type
    push_u64(out, flags); // sh_flags
    push_u64(out, addr); // sh_addr (vaddr for ALLOC sections, 0 for .debug_*)
    push_u64(out, offset); // sh_offset
    push_u64(out, size); // sh_size
    push_u32(out, 0); // sh_link
    push_u32(out, 0); // sh_info
    push_u64(out, addralign); // sh_addralign
    push_u64(out, 0); // sh_entsize
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
