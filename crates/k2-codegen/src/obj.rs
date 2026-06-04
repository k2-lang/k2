//! The pure-std ELF64 **relocatable object** (`ET_REL`) writer (v0.19 C interop).
//!
//! Where [`crate::elf`] emits a static, directly-runnable `ET_EXEC` with no
//! section headers (the freestanding path), this module emits a *linkable* object
//! for the C-interop path: a `.text` of stitched machine code with **no baked
//! absolute addresses**, a `.rodata` of string bytes, a `.symtab` (with the
//! `extern` callees as UNDEFINED symbols and the `export`/`main` functions as
//! defined GLOBAL symbols), and a `.rela.text` carrying the relocations the system
//! linker resolves. The finished object is handed to `cc`/`gcc` (the link driver)
//! to be linked against crt startup + libc into a dynamic executable (see
//! [`crate::link::compile_program_to_object`] and the driver's `--link-libc`).
//!
//! ## Why a separate writer
//!
//! The freestanding ELF bakes every `.rodata` pointer as an absolute `mov r64,
//! imm64` (the non-PIE load base is fixed) and every cross-fn `call` as an
//! in-image `rel32`. An object cannot do that: the final addresses are unknown
//! until the linker places the sections. Instead, the lowering's surviving
//! [`crate::encode::Fixup`]s are reinterpreted as ELF relocations:
//!
//! | fixup site                         | reloc type            | symbol        | addend          |
//! |------------------------------------|-----------------------|---------------|-----------------|
//! | `call` to an `extern` C function   | `R_X86_64_PLT32` (4)  | undef extern  | -4              |
//! | `call` to a defined fn (same obj)  | `R_X86_64_PLT32` (4)  | `.text` sect  | callee_off - 4  |
//! | `mov r64,imm64` `.rodata` pointer   | `R_X86_64_64` (1)     | `.rodata` sect | rodata offset   |
//!
//! The `R_X86_64_64` absolute form requires a **non-PIE** link (`cc -no-pie`),
//! which keeps the existing `mov imm64` instruction shape — the recommended v0.19
//! relocation mode. (A RIP-relative PC32 form for a default-PIE link is a localized
//! follow-up.)
//!
//! ## Section layout (file + header order)
//!
//! ```text
//!   [0] (null)        SHT_NULL
//!   [1] .text         SHT_PROGBITS  AX
//!   [2] .rodata       SHT_PROGBITS  A
//!   [3] .symtab       SHT_SYMTAB    (sh_link=.strtab, sh_info=first-global)
//!   [4] .strtab       SHT_STRTAB
//!   [5] .rela.text    SHT_RELA      (sh_link=.symtab, sh_info=.text)
//!   [6] .shstrtab     SHT_STRTAB
//! ```
//!
//! Everything is little-endian and hand-written; there are no external crates.

/// Section header indices, in the fixed order this writer emits them. Kept as
/// named constants so the symbol/relocation `st_shndx`/`sh_link`/`sh_info` cross
/// references read clearly.
const SHN_UNDEF: u16 = 0;
const SEC_TEXT: u16 = 1;
const SEC_RODATA: u16 = 2;
const SEC_SYMTAB: u16 = 3;
const SEC_STRTAB: u16 = 4;
const SEC_SHSTRTAB: u16 = 6;
/// The total number of section headers (including the null section).
const SEC_COUNT: u16 = 7;

// ELF section types.
const SHT_NULL: u32 = 0;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;

// ELF section flags.
const SHF_WRITE: u64 = 0x1;
const SHF_ALLOC: u64 = 0x2;
const SHF_EXECINSTR: u64 = 0x4;
const SHF_INFO_LINK: u64 = 0x40;

// Symbol binding / type (`st_info = (bind << 4) | type`).
const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STT_NOTYPE: u8 = 0;
const STT_FUNC: u8 = 2;
const STT_SECTION: u8 = 3;

// x86-64 relocation types.
const R_X86_64_64: u32 = 1;
const R_X86_64_PLT32: u32 = 4;

/// Sizes of the fixed ELF structures (bytes).
const EHDR_SIZE: u64 = 64;
const SHDR_SIZE: u64 = 64;
const SYM_SIZE: u64 = 24;
const RELA_SIZE: u64 = 24;

/// A symbol to emit into `.symtab`. The local/global split is implied by `bind`;
/// this writer orders all locals before all globals (an ELF requirement).
#[derive(Clone)]
pub(crate) struct ObjSymbol {
    /// The symbol name (`""` for the index-0 null symbol / a pure section symbol).
    pub name: String,
    /// `st_info` binding (`STB_LOCAL` / `STB_GLOBAL`).
    pub bind: u8,
    /// `st_info` type (`STT_FUNC` / `STT_NOTYPE` / `STT_SECTION`).
    pub typ: u8,
    /// `st_shndx`: the defining section index, or `SHN_UNDEF` for an extern.
    pub shndx: u16,
    /// `st_value`: the section-relative offset of a defined symbol (0 for undef).
    pub value: u64,
    /// `st_size`: the byte size of a defined function (0 otherwise).
    pub size: u64,
}

/// One relocation against `.text`. `sym` indexes into the symbol table built
/// alongside; `addend` is the RELA explicit addend.
#[derive(Clone, Copy)]
pub(crate) struct ObjReloc {
    /// The byte offset of the hole within `.text`.
    pub offset: u64,
    /// The index of the target symbol in the assembled `.symtab`.
    pub sym: u32,
    /// The relocation type (`R_X86_64_PLT32` / `R_X86_64_64`).
    pub typ: u32,
    /// The explicit RELA addend.
    pub addend: i64,
}

/// A finished relocatable object plus the facts the driver reports.
pub struct ObjectImage {
    /// The complete `.o` file bytes (write to disk, hand to `cc`).
    pub bytes: Vec<u8>,
    /// `true` if the object references any undefined `extern` symbol (so it must
    /// be linked against libc).
    pub had_extern: bool,
    /// The names of the defined `export`/`main` global symbols (for diagnostics).
    pub exports: Vec<String>,
}

/// Assembles an `ET_REL` object from the stitched `.text`, the `.rodata` blob, the
/// symbol list (locals first, then globals; index 0 must be the null symbol), and
/// the `.text` relocations. The section headers + string tables are built here.
pub(crate) fn write_object(
    text: &[u8],
    rodata: &[u8],
    symbols: &[ObjSymbol],
    relocs: &[ObjReloc],
    had_extern: bool,
    exports: Vec<String>,
) -> ObjectImage {
    // ---- Build the symbol string table (.strtab) + .symtab bytes. ----
    let mut strtab: Vec<u8> = vec![0]; // index 0 = empty string.
    let mut symtab: Vec<u8> = Vec::with_capacity(symbols.len() * SYM_SIZE as usize);
    let mut first_global: u32 = symbols.len() as u32; // default: no globals.
    for (i, s) in symbols.iter().enumerate() {
        if s.bind == STB_GLOBAL && (i as u32) < first_global {
            first_global = i as u32;
        }
        let name_off = if s.name.is_empty() {
            0
        } else {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(s.name.as_bytes());
            strtab.push(0);
            off
        };
        push_u32(&mut symtab, name_off); // st_name
        symtab.push((s.bind << 4) | s.typ); // st_info
        symtab.push(0); // st_other
        push_u16(&mut symtab, s.shndx); // st_shndx
        push_u64(&mut symtab, s.value); // st_value
        push_u64(&mut symtab, s.size); // st_size
    }

    // ---- Build the .rela.text bytes. ----
    let mut rela: Vec<u8> = Vec::with_capacity(relocs.len() * RELA_SIZE as usize);
    for r in relocs {
        push_u64(&mut rela, r.offset); // r_offset
        let r_info = ((r.sym as u64) << 32) | (r.typ as u64);
        push_u64(&mut rela, r_info); // r_info
        push_u64(&mut rela, r.addend as u64); // r_addend
    }

    // ---- Build the section-header string table (.shstrtab). ----
    let mut shstrtab: Vec<u8> = vec![0];
    let name_text = add_shstr(&mut shstrtab, ".text");
    let name_rodata = add_shstr(&mut shstrtab, ".rodata");
    let name_symtab = add_shstr(&mut shstrtab, ".symtab");
    let name_strtab = add_shstr(&mut shstrtab, ".strtab");
    let name_rela_text = add_shstr(&mut shstrtab, ".rela.text");
    let name_shstrtab = add_shstr(&mut shstrtab, ".shstrtab");

    // ---- Compute file offsets for each section's contents. ----
    // The ELF header is first; the section-header table is placed last (after all
    // section contents), with `e_shoff` pointing at it.
    let mut off = EHDR_SIZE;
    let text_off = align_up(off, 16);
    off = text_off + text.len() as u64;
    let rodata_off = align_up(off, 16);
    off = rodata_off + rodata.len() as u64;
    let symtab_off = align_up(off, 8);
    off = symtab_off + symtab.len() as u64;
    let strtab_off = off;
    off = strtab_off + strtab.len() as u64;
    let rela_off = align_up(off, 8);
    off = rela_off + rela.len() as u64;
    let shstrtab_off = off;
    off = shstrtab_off + shstrtab.len() as u64;
    let shoff = align_up(off, 8);

    let mut bytes: Vec<u8> = Vec::with_capacity((shoff + SEC_COUNT as u64 * SHDR_SIZE) as usize);

    // ---- ELF header (Ehdr, 64 bytes). ----
    bytes.extend_from_slice(&[0x7F, b'E', b'L', b'F']); // magic
    bytes.push(2); // ELFCLASS64
    bytes.push(1); // ELFDATA2LSB
    bytes.push(1); // EI_VERSION
    bytes.push(0); // ELFOSABI_SYSV
    bytes.push(0); // EI_ABIVERSION
    bytes.extend_from_slice(&[0; 7]); // padding
    push_u16(&mut bytes, 1); // e_type = ET_REL
    push_u16(&mut bytes, 0x3E); // e_machine = EM_X86_64
    push_u32(&mut bytes, 1); // e_version
    push_u64(&mut bytes, 0); // e_entry (none for an object)
    push_u64(&mut bytes, 0); // e_phoff (no program headers)
    push_u64(&mut bytes, shoff); // e_shoff
    push_u32(&mut bytes, 0); // e_flags
    push_u16(&mut bytes, EHDR_SIZE as u16); // e_ehsize
    push_u16(&mut bytes, 0); // e_phentsize
    push_u16(&mut bytes, 0); // e_phnum
    push_u16(&mut bytes, SHDR_SIZE as u16); // e_shentsize
    push_u16(&mut bytes, SEC_COUNT); // e_shnum
    push_u16(&mut bytes, SEC_SHSTRTAB); // e_shstrndx

    // ---- Section contents, each padded to its file offset. ----
    pad_to(&mut bytes, text_off);
    bytes.extend_from_slice(text);
    pad_to(&mut bytes, rodata_off);
    bytes.extend_from_slice(rodata);
    pad_to(&mut bytes, symtab_off);
    bytes.extend_from_slice(&symtab);
    pad_to(&mut bytes, strtab_off);
    bytes.extend_from_slice(&strtab);
    pad_to(&mut bytes, rela_off);
    bytes.extend_from_slice(&rela);
    pad_to(&mut bytes, shstrtab_off);
    bytes.extend_from_slice(&shstrtab);

    // ---- Section header table. ----
    pad_to(&mut bytes, shoff);
    // [0] null section.
    push_shdr(&mut bytes, 0, SHT_NULL, 0, 0, 0, 0, 0, 0);
    // [1] .text  (PROGBITS, ALLOC|EXEC).
    push_shdr(
        &mut bytes,
        name_text,
        SHT_PROGBITS,
        SHF_ALLOC | SHF_EXECINSTR,
        text_off,
        text.len() as u64,
        0,
        0,
        16,
    );
    // [2] .rodata (PROGBITS, ALLOC).
    push_shdr(
        &mut bytes,
        name_rodata,
        SHT_PROGBITS,
        SHF_ALLOC,
        rodata_off,
        rodata.len() as u64,
        0,
        0,
        16,
    );
    // [3] .symtab (SYMTAB; sh_link=.strtab, sh_info=first global, entsize=24).
    push_shdr_full(
        &mut bytes,
        name_symtab,
        SHT_SYMTAB,
        0,
        symtab_off,
        symtab.len() as u64,
        SEC_STRTAB as u32,
        first_global,
        8,
        SYM_SIZE,
    );
    // [4] .strtab (STRTAB).
    push_shdr(
        &mut bytes,
        name_strtab,
        SHT_STRTAB,
        0,
        strtab_off,
        strtab.len() as u64,
        0,
        0,
        1,
    );
    // [5] .rela.text (RELA; sh_link=.symtab, sh_info=.text, entsize=24).
    push_shdr_full(
        &mut bytes,
        name_rela_text,
        SHT_RELA,
        SHF_INFO_LINK,
        rela_off,
        rela.len() as u64,
        SEC_SYMTAB as u32,
        SEC_TEXT as u32,
        8,
        RELA_SIZE,
    );
    // [6] .shstrtab (STRTAB).
    push_shdr(
        &mut bytes,
        name_shstrtab,
        SHT_STRTAB,
        0,
        shstrtab_off,
        shstrtab.len() as u64,
        0,
        0,
        1,
    );

    ObjectImage {
        bytes,
        had_extern,
        exports,
    }
}

/// Appends a NUL-terminated section name to `.shstrtab` and returns its offset.
fn add_shstr(out: &mut Vec<u8>, name: &str) -> u32 {
    let off = out.len() as u32;
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    off
}

/// Appends one 64-byte `Elf64_Shdr` (the common case: `sh_link`/`sh_info` zero or
/// caller-provided, `sh_entsize` zero).
#[allow(clippy::too_many_arguments)]
fn push_shdr(
    out: &mut Vec<u8>,
    name: u32,
    typ: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
) {
    push_shdr_full(
        out, name, typ, flags, offset, size, link, info, addralign, 0,
    );
}

/// Appends one full 64-byte `Elf64_Shdr`, including `sh_entsize` (for `.symtab` /
/// `.rela.text`, whose entries have a fixed size).
#[allow(clippy::too_many_arguments)]
fn push_shdr_full(
    out: &mut Vec<u8>,
    name: u32,
    typ: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
) {
    push_u32(out, name); // sh_name
    push_u32(out, typ); // sh_type
    push_u64(out, flags); // sh_flags
    push_u64(out, 0); // sh_addr (0 in a relocatable object)
    push_u64(out, offset); // sh_offset
    push_u64(out, size); // sh_size
    push_u32(out, link); // sh_link
    push_u32(out, info); // sh_info
    push_u64(out, addralign); // sh_addralign
    push_u64(out, entsize); // sh_entsize
}

/// Re-exports the writable section flag so the link pass can request a `.data`
/// section in a future revision without re-deriving the constant. Currently unused
/// because the corpus's FFI programs have no writable globals; kept documented.
#[allow(dead_code)]
pub(crate) const WRITABLE: u64 = SHF_WRITE;

/// Re-exported relocation/symbol constants the link pass uses when constructing
/// [`ObjSymbol`]/[`ObjReloc`] values, so the magic numbers live in one place.
pub(crate) mod abi {
    pub(crate) const SHN_UNDEF: u16 = super::SHN_UNDEF;
    pub(crate) const SEC_TEXT: u16 = super::SEC_TEXT;
    pub(crate) const SEC_RODATA: u16 = super::SEC_RODATA;
    pub(crate) const STB_LOCAL: u8 = super::STB_LOCAL;
    pub(crate) const STB_GLOBAL: u8 = super::STB_GLOBAL;
    pub(crate) const STT_NOTYPE: u8 = super::STT_NOTYPE;
    pub(crate) const STT_FUNC: u8 = super::STT_FUNC;
    pub(crate) const STT_SECTION: u8 = super::STT_SECTION;
    pub(crate) const R_X86_64_64: u32 = super::R_X86_64_64;
    pub(crate) const R_X86_64_PLT32: u32 = super::R_X86_64_PLT32;
}

/// Rounds `v` up to the next multiple of `align` (a power of two).
fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// Zero-pads `out` up to file offset `to`.
fn pad_to(out: &mut Vec<u8>, to: u64) {
    if (out.len() as u64) < to {
        out.resize(to as usize, 0);
    }
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
