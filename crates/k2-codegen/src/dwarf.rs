//! The pure-std DWARF v5 emitter for the native x86-64 backend (v0.27).
//!
//! This module hand-encodes the four DWARF sections a source-level debugger
//! reads — `.debug_abbrev`, `.debug_str`, `.debug_line`, and `.debug_info` — from
//! the facts the link pass already knows: each emitted function's name, `.text`
//! address range (`low_pc`/`high_pc`), and defining line, plus a per-function
//! sequence of `(code address, source line)` rows derived from the MIR
//! per-statement [`k2_syntax::Span`]s. The result is a [`crate::elf::DebugSections`]
//! the ELF writer drops into **unmapped** `SHT_PROGBITS` sections; nothing here
//! touches the executed code.
//!
//! ## Why hand-rolled
//!
//! The whole k2 toolchain depends on `std` only — there is no `gimli`, no
//! `object`, no LLVM. So every byte of DWARF is emitted by the little-endian /
//! LEB128 builders below and validated against the *oracle* tools available in
//! this environment (`llvm-dwarfdump --verify`, `addr2line`, `readelf`). There is
//! no gdb here; live source-level debugging is expected to work under gdb/lldb on
//! a host that has them, because they consume the same DWARF those oracle tools
//! validate. See `docs/dwarf.md`.
//!
//! ## DWARF flavor
//!
//! **DWARF v5, DWARF32, 8-byte addresses.** v5 is chosen deliberately: it makes
//! the primary source file index **0** in both the line program and the
//! `DW_AT_decl_file` attribute, so `addr2line` reports the source basename
//! directly, and LLVM 21's `llvm-dwarfdump` is fully v5-native. The file table is
//! multi-entry and file-aware: index 0 is the primary user source and the
//! remaining entries are the distinct `@import`-ed modules a multi-file program
//! draws inlined code from, so every address resolves to its true `(file, line)`.
//! `DW_AT_high_pc` is emitted in the modern **offset form** (`DW_FORM_data8`,
//! `high_pc = low_pc + value`), which both LLVM and `addr2line` expect.
//!
//! ## Section contents
//!
//! * `.debug_abbrev` — two abbreviation declarations: a `DW_TAG_compile_unit`
//!   (with children) and a `DW_TAG_subprogram` (leaf).
//! * `.debug_str` — a `\0`-led string pool referenced via `DW_FORM_strp`.
//! * `.debug_line` — a v5 line-number program: one **sequence per function**
//!   (each ended by `DW_LNE_end_sequence`), mapping code address ranges to source
//!   lines. One sequence per function keeps `addr2line` from interpolating across
//!   the gaps between functions (and over the source-less `_start`/runtime
//!   prelude, which are simply omitted). The line-program's v5 file table has one
//!   entry **per distinct source file** the program draws code from (the user's
//!   file plus any `@import`-ed module such as the bundled `std`), and each
//!   sequence selects its own file with `DW_LNS_set_file` so an inlined std
//!   function's addresses resolve to *its* real line in *its* file — never to a
//!   nonexistent line of the main file.
//! * `.debug_info` — the compilation-unit DIE (`producer`/`name`/`comp_dir`/
//!   `low_pc`/`high_pc`/`stmt_list`) and one `DW_TAG_subprogram` DIE per emitted
//!   function (`name`/`low_pc`/`high_pc`/`decl_file`/`decl_line`), with
//!   `decl_file` carrying that function's own file-table index.

// The DWARF constants below keep their canonical spec spellings (`DW_TAG_*`,
// `DW_AT_*`, `DW_FORM_*`, `DW_LNS_*`, …) for one-to-one spec/dwarfdump fidelity;
// see the comment on the constant block. Allowed crate-spelling lint scoped here.
#![allow(non_upper_case_globals)]

// DWARF constants (DWARF v5 §7). Only the subset this emitter uses is named.
// These deliberately keep the DWARF spec's canonical `DW_TAG_*` / `DW_AT_*` /
// `DW_FORM_*` identifiers verbatim, so anyone cross-referencing the spec or
// `llvm-dwarfdump` output finds the exact names. The spelling is the established
// convention across every DWARF producer; renaming to SCREAMING_SNAKE_CASE would
// obscure that mapping. The `non_upper_case_globals` lint is therefore allowed for
// the constant declarations below (justified — spec fidelity).

// Tags.
const DW_TAG_compile_unit: u8 = 0x11;
const DW_TAG_subprogram: u8 = 0x2e;

// Attributes.
const DW_AT_name: u8 = 0x03;
const DW_AT_stmt_list: u8 = 0x10;
const DW_AT_low_pc: u8 = 0x11;
const DW_AT_high_pc: u8 = 0x12;
const DW_AT_language: u8 = 0x13;
const DW_AT_comp_dir: u8 = 0x1b;
const DW_AT_producer: u8 = 0x25;
const DW_AT_decl_file: u8 = 0x3a;
const DW_AT_decl_line: u8 = 0x3b;

// Forms.
const DW_FORM_addr: u8 = 0x01;
const DW_FORM_data2: u8 = 0x05;
const DW_FORM_data8: u8 = 0x07;
const DW_FORM_strp: u8 = 0x0e;
const DW_FORM_udata: u8 = 0x0f;
const DW_FORM_sec_offset: u8 = 0x17;

// `unit_type` (DWARF v5 §7.5.1).
const DW_UT_compile: u8 = 0x01;

// Source language. `DW_LANG_C` (0x0002) is the most tool-compatible value for a
// systems language with no dedicated code; `llvm-dwarfdump` prints it as
// `DW_LANG_C` and gdb treats line/scope info the same.
const DW_LANG_C: u16 = 0x0002;

// Line-number standard opcodes.
const DW_LNS_copy: u8 = 0x01;
const DW_LNS_advance_pc: u8 = 0x02;
const DW_LNS_advance_line: u8 = 0x03;
const DW_LNS_set_file: u8 = 0x04;

// Line-number extended opcodes.
const DW_LNE_end_sequence: u8 = 0x01;
const DW_LNE_set_address: u8 = 0x02;

// Line program v5 directory/file entry-format content types + forms.
const DW_LNCT_path: u8 = 0x1;
const DW_LNCT_directory_index: u8 = 0x2;
const DW_FORM_string: u8 = 0x08;

// Line-number program tuning. `line_base`/`line_range`/`opcode_base` follow the
// values LLVM/GCC emit, so the special-opcode arithmetic below matches the
// reference encoders and `addr2line`'s decoder.
const LINE_BASE: i8 = -5;
const LINE_RANGE: u8 = 14;
const OPCODE_BASE: u8 = 13;

/// One emitted function's DWARF facts: its display name, absolute `.text`
/// address range, the source line of its `fn` keyword, and which source file that
/// line belongs to.
pub struct DwFn {
    /// The function's display name (`main`, `List(u32).push`, …).
    pub name: String,
    /// The absolute virtual address of the function's first byte (its `low_pc`).
    pub low_pc: u64,
    /// The function's byte length (its `high_pc` is `low_pc + len`).
    pub len: u64,
    /// The 0-based index into [`DwarfInput::files`] of the file this function is
    /// defined in (its `DW_AT_decl_file`). For a single-file program this is
    /// always 0; for a program that inlines `@import`-ed code each function points
    /// at *its own* source file.
    pub file: u32,
    /// The 1-based source line of the function's definition, **in its own file**
    /// (i.e. already translated back from any merged-source line number).
    pub decl_line: u32,
}

/// One `(address, file, line)` row of the line-number table. The terminating row
/// of a function's sequence has `end_sequence = true` and an `address` one past
/// the function's last byte.
pub struct DwRow {
    /// The absolute virtual code address this row begins at.
    pub address: u64,
    /// The 0-based index into [`DwarfInput::files`] of the source file this row's
    /// line belongs to. Rows of one function may differ (a function can inline code
    /// from more than one file); the line program emits a `DW_LNS_set_file` each
    /// time the file changes between consecutive rows.
    pub file: u32,
    /// The 1-based source line (in this row's file; ignored when `end_sequence`).
    pub line: u32,
    /// `true` for the sequence-terminating row.
    pub end_sequence: bool,
}

/// One per-function line-number **sequence**: a monotonically increasing run of
/// rows ended by an `end_sequence` row. Each function is its own sequence so
/// `addr2line` never interpolates a line across the gap between two functions.
pub struct DwSeq {
    /// The rows, address-ascending; the last has `end_sequence = true`. Each row
    /// carries its own file (see [`DwRow::file`]).
    pub rows: Vec<DwRow>,
}

/// One entry in the line program's v5 file-name table: a path plus the index of
/// the directory it lives under (always 0 — the single `comp_dir` directory — for
/// the basenames this emitter records).
pub struct DwFile {
    /// The file's path as `addr2line`/`llvm-dwarfdump` should report it (a
    /// basename such as `hello.k2` or `std.k2`).
    pub path: String,
    /// The 0-based directory-table index this file lives under (0 = `comp_dir`).
    pub dir_index: u32,
}

/// Everything [`build`] needs to emit the four debug sections.
pub struct DwarfInput<'a> {
    /// The `DW_AT_producer` string (e.g. `"k2 v0.27"`).
    pub producer: &'a str,
    /// The line program's v5 file table, in index order. Entry 0 is the **primary
    /// source** (it also becomes the CU's `DW_AT_name`); the rest are the distinct
    /// `@import`-ed files inlined code is attributed to. Must be non-empty.
    pub files: Vec<DwFile>,
    /// The compilation directory (`DW_AT_comp_dir` / the line table's dir 0).
    pub comp_dir: &'a str,
    /// The virtual address of `.text` (the CU's `low_pc`).
    pub text_vaddr: u64,
    /// The total `.text` length (the CU's `high_pc` offset).
    pub text_len: u64,
    /// One entry per emitted function (with a real source location).
    pub funcs: Vec<DwFn>,
    /// The per-function line-number sequences.
    pub lines: Vec<DwSeq>,
}

impl DwarfInput<'_> {
    /// The primary source file's path (file index 0), the CU's `DW_AT_name`. Falls
    /// back to an empty string if the file table is somehow empty (it never is on
    /// the production path — [`build`] is total and must not panic).
    fn primary_name(&self) -> &str {
        self.files.first().map(|f| f.path.as_str()).unwrap_or("")
    }
}

/// Builds the four DWARF v5 sections from `input`.
///
/// The encoding is purely a function of the input facts — there is no I/O and no
/// host dependency — so the whole emitter is byte-level unit-testable on any
/// platform (see `tests::dwarf`). It never panics on any input.
pub fn build(input: &DwarfInput) -> crate::elf::DebugSections {
    let mut strs = StrTab::new();
    let producer = strs.add(input.producer);
    let name = strs.add(input.primary_name());
    let comp_dir = strs.add(input.comp_dir);
    let fn_name_offs: Vec<u32> = input.funcs.iter().map(|f| strs.add(&f.name)).collect();

    let abbrev = build_abbrev();
    let line = build_line(input);
    let info = build_info(input, producer, name, comp_dir, &fn_name_offs);

    crate::elf::DebugSections {
        info,
        abbrev,
        line,
        str_: strs.into_bytes(),
    }
}

// ===========================================================================
//  .debug_abbrev
// ===========================================================================

/// Builds the abbreviation table: code 1 = `DW_TAG_compile_unit` (has children),
/// code 2 = `DW_TAG_subprogram` (leaf), terminated by a `0` byte. Each DIE in
/// `.debug_info` references one of these codes, and the attribute (form) order
/// here must match the attribute (value) order emitted there exactly.
fn build_abbrev() -> Vec<u8> {
    let mut b = Vec::new();

    // abbrev 1: DW_TAG_compile_unit, has_children = 1.
    uleb(&mut b, 1);
    b.push(DW_TAG_compile_unit);
    b.push(1); // DW_CHILDREN_yes
    attr(&mut b, DW_AT_producer, DW_FORM_strp);
    attr(&mut b, DW_AT_language, DW_FORM_data2);
    attr(&mut b, DW_AT_name, DW_FORM_strp);
    attr(&mut b, DW_AT_comp_dir, DW_FORM_strp);
    attr(&mut b, DW_AT_low_pc, DW_FORM_addr);
    attr(&mut b, DW_AT_high_pc, DW_FORM_data8);
    attr(&mut b, DW_AT_stmt_list, DW_FORM_sec_offset);
    attr(&mut b, 0, 0); // end of attribute list

    // abbrev 2: DW_TAG_subprogram, has_children = 0.
    uleb(&mut b, 2);
    b.push(DW_TAG_subprogram);
    b.push(0); // DW_CHILDREN_no
    attr(&mut b, DW_AT_name, DW_FORM_strp);
    attr(&mut b, DW_AT_low_pc, DW_FORM_addr);
    attr(&mut b, DW_AT_high_pc, DW_FORM_data8);
    // `DW_AT_decl_file` as ULEB (`DW_FORM_udata`) rather than a single byte: the
    // file-table index is now per-function and a large `@import` graph can carry
    // more than 255 files, so a 1-byte form could overflow. ULEB is exact and what
    // LLVM emits for v5 too.
    attr(&mut b, DW_AT_decl_file, DW_FORM_udata);
    attr(&mut b, DW_AT_decl_line, DW_FORM_udata);
    attr(&mut b, 0, 0); // end of attribute list

    b.push(0); // end of the abbreviation table
    b
}

/// Appends one `(attribute, form)` ULEB128 pair to an abbreviation declaration.
fn attr(b: &mut Vec<u8>, at: u8, form: u8) {
    uleb(b, at as u64);
    uleb(b, form as u64);
}

// ===========================================================================
//  .debug_info
// ===========================================================================

/// Builds the compilation-unit + subprogram DIE tree. The CU header carries the
/// `DWARF32` `unit_length` (back-patched once the body is known), `version = 5`,
/// `unit_type = DW_UT_compile`, the 8-byte address size, and a 0
/// `debug_abbrev_offset` (single CU → the abbrev table starts at 0).
fn build_info(
    input: &DwarfInput,
    producer: u32,
    name: u32,
    comp_dir: u32,
    fn_name_offs: &[u32],
) -> Vec<u8> {
    let mut b = Vec::new();

    // unit_length placeholder (DWARF32): patched after the body is serialized.
    let len_fixup = b.len();
    push_u32(&mut b, 0);
    let body_start = b.len();

    push_u16(&mut b, 5); // version
    b.push(DW_UT_compile); // unit_type
    b.push(8); // address_size
    push_u32(&mut b, 0); // debug_abbrev_offset

    // ---- The compile-unit DIE (abbrev 1). ----
    uleb(&mut b, 1);
    push_u32(&mut b, producer); // DW_AT_producer (strp)
    push_u16(&mut b, DW_LANG_C); // DW_AT_language (data2)
    push_u32(&mut b, name); // DW_AT_name (strp)
    push_u32(&mut b, comp_dir); // DW_AT_comp_dir (strp)
    push_u64(&mut b, input.text_vaddr); // DW_AT_low_pc (addr)
    push_u64(&mut b, input.text_len); // DW_AT_high_pc (data8 offset)
    push_u32(&mut b, 0); // DW_AT_stmt_list (sec_offset into .debug_line)

    // ---- One subprogram DIE per emitted function (abbrev 2). ----
    for (f, &name_off) in input.funcs.iter().zip(fn_name_offs) {
        uleb(&mut b, 2);
        push_u32(&mut b, name_off); // DW_AT_name (strp)
        push_u64(&mut b, f.low_pc); // DW_AT_low_pc (addr)
        push_u64(&mut b, f.len); // DW_AT_high_pc (data8 offset)
        uleb(&mut b, f.file as u64); // DW_AT_decl_file (udata) — this fn's own file
        uleb(&mut b, f.decl_line as u64); // DW_AT_decl_line (udata)
    }

    b.push(0); // end of the CU's children

    // Back-patch unit_length = total bytes after the length field itself.
    let unit_len = (b.len() - body_start) as u32;
    b[len_fixup..len_fixup + 4].copy_from_slice(&unit_len.to_le_bytes());
    b
}

// ===========================================================================
//  .debug_line  (DWARF v5 line-number program)
// ===========================================================================

/// Builds the v5 line-number program: the unit header (with the v5 directory/file
/// entry-format tables), then one sequence per function. `unit_length` and
/// `header_length` are back-patched after their respective spans are serialized.
fn build_line(input: &DwarfInput) -> Vec<u8> {
    let mut b = Vec::new();

    // unit_length placeholder (DWARF32).
    let unit_len_fixup = b.len();
    push_u32(&mut b, 0);
    let unit_body_start = b.len();

    push_u16(&mut b, 5); // version
    b.push(8); // address_size
    b.push(0); // segment_selector_size

    // header_length placeholder: bytes from *after this field* to the start of
    // the line-number program.
    let header_len_fixup = b.len();
    push_u32(&mut b, 0);
    let header_start = b.len();

    b.push(1); // minimum_instruction_length
    b.push(1); // maximum_operations_per_instruction
    b.push(1); // default_is_stmt
    b.push(LINE_BASE as u8); // line_base
    b.push(LINE_RANGE); // line_range
    b.push(OPCODE_BASE); // opcode_base
                         // standard_opcode_lengths[opcode_base - 1] — the DWARF-standard argument
                         // counts for opcodes 1..=12.
    for n in [0u8, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1] {
        b.push(n);
    }

    // ---- v5 directory table: one directory (the comp_dir), as a string. ----
    b.push(1); // directory_entry_format_count
    uleb(&mut b, DW_LNCT_path as u64);
    uleb(&mut b, DW_FORM_string as u64);
    uleb(&mut b, 1); // directories_count
    cstr(&mut b, input.comp_dir); // directory 0

    // ---- v5 file table: one real entry PER DISTINCT SOURCE FILE. ----
    // DWARF v5 makes file index 0 the primary source. The historical (v0.27)
    // emitter duplicated that one file at index 0 *and* index 1 and let every
    // sequence keep the default file register (1) for GNU-binutils compatibility —
    // but that assumes a single source file, so an `@import`-ed function's inlined
    // code was mis-attributed to the main file's (nonexistent) lines. We now emit a
    // genuine, file-aware table: index 0 is the user's source and each further entry
    // is a distinct imported file (e.g. the bundled `std`). Every sequence selects
    // its own file explicitly with `DW_LNS_set_file` (see `emit_sequence`), so no
    // default-file convention is relied on, the duplicate-file-1 entry is gone (and
    // with it `llvm-dwarfdump`'s "duplicate of file_names[0]" warning), and both GNU
    // and LLVM `addr2line` resolve every address to its true file and line.
    b.push(2); // file_name_entry_format_count
    uleb(&mut b, DW_LNCT_path as u64);
    uleb(&mut b, DW_FORM_string as u64);
    uleb(&mut b, DW_LNCT_directory_index as u64);
    uleb(&mut b, DW_FORM_udata as u64);
    uleb(&mut b, input.files.len() as u64); // file_names_count
    for file in &input.files {
        cstr(&mut b, &file.path); // file path
        uleb(&mut b, file.dir_index as u64); // file directory index
    }

    // Back-patch header_length now that the program is about to start.
    let header_len = (b.len() - header_start) as u32;
    b[header_len_fixup..header_len_fixup + 4].copy_from_slice(&header_len.to_le_bytes());

    // ---- The line-number program: one sequence per function. ----
    for seq in &input.lines {
        emit_sequence(&mut b, seq);
    }

    // Back-patch unit_length.
    let unit_len = (b.len() - unit_body_start) as u32;
    b[unit_len_fixup..unit_len_fixup + 4].copy_from_slice(&unit_len.to_le_bytes());
    b
}

/// Emits one line-number sequence: a `DW_LNE_set_address` to the first row, a row
/// for each `(address, file, line)` (a `DW_LNS_set_file` first whenever the file
/// differs from the current register, then a special opcode when the deltas fit or
/// the explicit `advance_line`/`advance_pc`/`copy` triple), and a terminating
/// `DW_LNE_end_sequence` at the sequence's end address.
fn emit_sequence(b: &mut Vec<u8>, seq: &DwSeq) {
    // The line-number state machine's defaults at the start of a sequence:
    // address = 0 (we set it explicitly), line = 1, file = 1. We track the file
    // register and emit `DW_LNS_set_file` whenever a row's file differs, so a
    // function that inlines code from several files attributes each row to its own
    // file (never the wrong file's nonexistent line). The first row's file is set
    // explicitly too (unless it is the default 1), so nothing depends on the file
    // register's reset value across readers.
    let mut cur_addr: u64 = 0;
    let mut cur_line: i64 = 1;
    let mut cur_file: u32 = 1; // the per-spec default at sequence start
    let mut started = false;

    for row in &seq.rows {
        if row.end_sequence {
            // Advance the PC to the end address, then terminate.
            if row.address > cur_addr {
                b.push(DW_LNS_advance_pc);
                uleb(b, row.address - cur_addr);
                cur_addr = row.address;
            }
            // DW_LNE_end_sequence: extended opcode, length 1.
            b.push(0);
            uleb(b, 1);
            b.push(DW_LNE_end_sequence);
            continue;
        }

        // Select this row's file if it changed (also covers the first row).
        if row.file != cur_file {
            b.push(DW_LNS_set_file);
            uleb(b, row.file as u64);
            cur_file = row.file;
        }

        if !started {
            // DW_LNE_set_address <u64>: extended opcode, length 9.
            b.push(0);
            uleb(b, 9);
            b.push(DW_LNE_set_address);
            push_u64(b, row.address);
            cur_addr = row.address;
            started = true;
        }

        let daddr = row.address - cur_addr;
        let dline = row.line as i64 - cur_line;
        emit_row(b, daddr, dline);
        cur_addr = row.address;
        cur_line = row.line as i64;
    }
}

/// Appends one row that advances the PC by `daddr` and the line by `dline`,
/// preferring a single special opcode and falling back to the explicit
/// `advance_line`/`advance_pc` + `DW_LNS_copy` form when the deltas don't fit.
fn emit_row(b: &mut Vec<u8>, daddr: u64, dline: i64) {
    // A special opcode encodes both deltas in one byte when:
    //   line_base <= dline <= line_base + line_range - 1   (i.e. -5..=8), and
    //   opcode = (dline - line_base) + line_range*daddr + opcode_base <= 255.
    let line_lo = LINE_BASE as i64;
    let line_hi = LINE_BASE as i64 + LINE_RANGE as i64 - 1;
    if dline >= line_lo && dline <= line_hi {
        let op = (dline - line_lo) + LINE_RANGE as i64 * daddr as i64 + OPCODE_BASE as i64;
        if (OPCODE_BASE as i64..=255).contains(&op) {
            b.push(op as u8);
            return;
        }
    }
    // Fallback: advance line and PC explicitly, then DW_LNS_copy.
    if dline != 0 {
        b.push(DW_LNS_advance_line);
        sleb(b, dline);
    }
    if daddr != 0 {
        b.push(DW_LNS_advance_pc);
        uleb(b, daddr);
    }
    b.push(DW_LNS_copy);
}

// ===========================================================================
//  .debug_str
// ===========================================================================

/// A `\0`-led string pool: interning a string appends it (NUL-terminated) and
/// returns its byte offset, the value a `DW_FORM_strp` attribute carries.
/// Identical strings are deduplicated (a size win that never changes semantics).
struct StrTab {
    bytes: Vec<u8>,
    seen: std::collections::HashMap<String, u32>,
}

impl StrTab {
    /// A fresh pool whose offset 0 is the empty string (`\0`).
    fn new() -> StrTab {
        StrTab {
            bytes: vec![0],
            seen: std::collections::HashMap::new(),
        }
    }

    /// Interns `s` (deduplicated) and returns its `.debug_str` offset.
    fn add(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.seen.get(s) {
            return off;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        self.seen.insert(s.to_string(), off);
        off
    }

    /// The finished pool bytes.
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

// ===========================================================================
//  Low-level byte builders (LEB128 + little-endian).
// ===========================================================================

/// Appends `s` as a NUL-terminated byte string (a `DW_FORM_string` value).
fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// Appends an unsigned LEB128 (DWARF §7.6).
fn uleb(b: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        b.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Appends a signed LEB128 (DWARF §7.6).
fn sleb(b: &mut Vec<u8>, mut v: i64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7; // arithmetic shift (sign-propagating)
        let sign_bit = byte & 0x40 != 0;
        let more = !((v == 0 && !sign_bit) || (v == -1 && sign_bit));
        if more {
            byte |= 0x80;
        }
        b.push(byte);
        if !more {
            break;
        }
    }
}

/// Appends a little-endian `u16`.
fn push_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
/// Appends a little-endian `u32`.
fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
/// Appends a little-endian `u64`.
fn push_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod unit {
    //! Round-trip + invariant tests for the LEB128 builders that the section
    //! encoders rely on. The section-level byte assertions live in
    //! `crate::tests::dwarf` alongside the rest of the codegen tests.

    use super::*;

    /// Decodes a ULEB128 from `b[*i..]`, advancing `i`. Test-only oracle.
    fn read_uleb(b: &[u8], i: &mut usize) -> u64 {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = b[*i];
            *i += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        result
    }

    /// Decodes an SLEB128 from `b[*i..]`, advancing `i`. Test-only oracle.
    fn read_sleb(b: &[u8], i: &mut usize) -> i64 {
        let mut result: i64 = 0;
        let mut shift = 0;
        let mut byte;
        loop {
            byte = b[*i];
            *i += 1;
            result |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        if shift < 64 && (byte & 0x40) != 0 {
            result |= -1i64 << shift;
        }
        result
    }

    #[test]
    fn uleb_roundtrip() {
        for v in [
            0u64,
            1,
            2,
            63,
            64,
            127,
            128,
            300,
            624_485,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut b = Vec::new();
            uleb(&mut b, v);
            let mut i = 0;
            assert_eq!(read_uleb(&b, &mut i), v, "uleb {v}");
            assert_eq!(i, b.len(), "uleb {v} length");
        }
    }

    #[test]
    fn sleb_roundtrip() {
        for v in [
            0i64,
            1,
            -1,
            2,
            -2,
            63,
            64,
            -64,
            -65,
            127,
            -128,
            129,
            -129,
            i32::MIN as i64,
            i64::MAX,
            i64::MIN,
        ] {
            let mut b = Vec::new();
            sleb(&mut b, v);
            let mut i = 0;
            assert_eq!(read_sleb(&b, &mut i), v, "sleb {v}");
            assert_eq!(i, b.len(), "sleb {v} length");
        }
    }

    #[test]
    fn uleb_known_encodings() {
        // From the DWARF spec's worked examples.
        let mut b = Vec::new();
        uleb(&mut b, 2);
        assert_eq!(b, [0x02]);
        b.clear();
        uleb(&mut b, 127);
        assert_eq!(b, [0x7f]);
        b.clear();
        uleb(&mut b, 128);
        assert_eq!(b, [0x80, 0x01]);
        b.clear();
        uleb(&mut b, 624_485);
        assert_eq!(b, [0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn sleb_known_encodings() {
        let mut b = Vec::new();
        sleb(&mut b, 2);
        assert_eq!(b, [0x02]);
        b.clear();
        sleb(&mut b, -2);
        assert_eq!(b, [0x7e]);
        b.clear();
        sleb(&mut b, 127);
        assert_eq!(b, [0xff, 0x00]);
        b.clear();
        sleb(&mut b, -128);
        assert_eq!(b, [0x80, 0x7f]);
    }
}
