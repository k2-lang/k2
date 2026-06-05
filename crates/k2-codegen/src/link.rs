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

use k2_mir::{FnId, Linkage, MirProgram};

use crate::aarch64;
use crate::dwarf;
use crate::elf::{self, ElfImage};
use crate::encode::{Asm, FixupKind, LineMark};
use crate::lower::FnLower;
use crate::obj::{self, ObjReloc, ObjSymbol, ObjectImage};
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
    /// Surviving DWARF line marks (post-peephole, `at` relative to `code[0]`),
    /// collected only on the `-g` path; empty otherwise.
    line_marks: Vec<LineMark>,
    /// The byte offset of this function within the assembled `.text` (filled in
    /// during stitching).
    text_off: usize,
}

/// The source-location context the `-g` DWARF path needs: the source file the
/// program was compiled from, the compilation directory, and an optional
/// merged-line → true-`(file, line)` map.
///
/// `src_path`/`comp_dir` become the compilation unit's `DW_AT_name`/`DW_AT_comp_dir`
/// and the line table's primary file/directory entries. `source_map`, when
/// present, makes DWARF **file-aware**: the driver compiles a single MERGED source
/// (the user's file plus the appended `std` prelude and any `@import`-ed module),
/// so a function's `Span.line` is a line of that merged text — which for an
/// imported function is NOT a line of the user's file. The map recovers each merged
/// line's true originating `(file, line)`, so the line table and `DW_AT_decl_file`
/// point at the real file (the `std` source, say) at its real line instead of a
/// nonexistent line of the main file. With no map (or a line the map cannot place)
/// the location falls back to the primary file at its raw line, which is correct
/// for a genuinely single-file program.
#[derive(Clone, Default)]
pub struct DebugCtx {
    /// The source file path as given on the command line (its basename becomes the
    /// CU/line-table file name; an absolute path is recorded verbatim).
    pub src_path: String,
    /// The compilation directory (`std::env::current_dir()`), recorded as
    /// `DW_AT_comp_dir` / the line table's directory 0.
    pub comp_dir: String,
    /// The merged-source line map (see the type-level docs). Empty by default,
    /// in which case the build behaves exactly as the original single-file path.
    pub source_map: DwarfSourceMap,
}

/// A line-level map from a merged-text line (1-based) back to the original
/// `(file display name, file line)` it came from, for the file-aware DWARF path.
///
/// This mirrors the driver's `multi::SourceMap` but lives in the codegen crate (no
/// cross-crate dependency): the driver fills it in from the same segment facts it
/// already records when it appends the `std` prelude / merges imported modules.
/// A merged line not covered by any segment (synthesized scaffolding — a wrapper
/// `struct {` header, an empty options root, …) resolves to `None`, and the caller
/// keeps the primary file at the raw line, which never mis-points at a foreign
/// file.
#[derive(Clone, Debug, Default)]
pub struct DwarfSourceMap {
    /// Segments in ascending merged-line order. Each is
    /// `(merged_start_line, line_count, file_display, file_start_line)`: merged
    /// lines `[merged_start_line, merged_start_line + line_count)` map to
    /// `file_display` lines starting at `file_start_line`.
    segments: Vec<(u32, u32, String, u32)>,
}

impl DwarfSourceMap {
    /// Builds a map from raw segment tuples
    /// `(merged_start_line, line_count, file_display, file_start_line)`.
    pub fn from_segments(segments: Vec<(u32, u32, String, u32)>) -> DwarfSourceMap {
        DwarfSourceMap { segments }
    }

    /// `true` if no segments were recorded (the single-file fast path).
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Recovers the original `(file, line)` for a merged line (1-based), or `None`
    /// if the line falls in synthesized scaffolding that maps to no real source.
    fn resolve(&self, merged_line: u32) -> Option<(&str, u32)> {
        for (start, count, file, file_start) in &self.segments {
            if merged_line >= *start && merged_line < start + count {
                return Some((file.as_str(), file_start + (merged_line - start)));
            }
        }
        None
    }
}

/// Compiles a whole [`MirProgram`] to a runnable ELF image for `target`, or fails
/// with a [`CodegenError`] if any reached function is outside the selected
/// target's subset (the error names the offending construct).
///
/// The x86-64 path is the original pipeline, byte-for-byte. The aarch64 path
/// (cross-compilation only) uses the [`crate::aarch64`] encoder + lowering and is
/// dispatched separately because its `_start` shim and relocation widths differ.
pub fn compile_program(prog: &MirProgram, target: Target) -> Result<ElfImage, CodegenError> {
    compile_program_with_debug(prog, target, None)
}

/// Compiles a whole [`MirProgram`] to a runnable ELF image for `target`,
/// optionally emitting DWARF v5 debug info (`debug = Some`). DWARF emission is
/// x86-64-freestanding only; on aarch64 (or with `debug = None`) the plain path
/// runs and `debug` is ignored.
pub fn compile_program_with_debug(
    prog: &MirProgram,
    target: Target,
    debug: Option<&DebugCtx>,
) -> Result<ElfImage, CodegenError> {
    match target {
        Target::X86_64Linux => compile_program_x86(prog, debug),
        // aarch64 DWARF is deferred (v0.27 scope: x86-64 freestanding only). The
        // cross-compiled binary is still emitted, just without `.debug_*`.
        Target::Aarch64Linux => aarch64::link::compile_program_aarch64(prog),
    }
}

/// Compiles a whole [`MirProgram`] to a runnable x86-64 ELF image. When `debug` is
/// `Some`, the same `.text`/`.rodata`/segments are emitted (byte-identical to the
/// `None` path) plus trailing, unmapped DWARF `.debug_*` sections + a
/// section-header table.
fn compile_program_x86(
    prog: &MirProgram,
    debug: Option<&DebugCtx>,
) -> Result<ElfImage, CodegenError> {
    let main_id = find_main(prog).ok_or(CodegenError::NoMain)?;

    // ---- Lower every function, collecting code + fixups (+ line marks on -g). ----
    let mut rodata = RoData::new();
    let mut lowered: Vec<LoweredFn> = Vec::with_capacity(prog.funcs.len());
    for func in &prog.funcs {
        let (code, fixups, line_marks) = if debug.is_some() {
            FnLower::new(prog, func).lower_with_lines(&mut rodata)?
        } else {
            let (code, fixups) = FnLower::new(prog, func).lower(&mut rodata)?;
            (code, fixups, Vec::new())
        };
        lowered.push(LoweredFn {
            id: func.id,
            code,
            fixups,
            line_marks,
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

    // ---- (debug only) Build the DWARF sections from the stitched layout. ----
    // The text bytes are final (every fixup is patched), so each function's
    // absolute address range and per-statement line rows are now known. DWARF is
    // pure trailing metadata: it reads `text`/`lowered` but never modifies them.
    let debug_sections =
        debug.map(|ctx| build_dwarf(prog, &lowered, text.len() as u64, elf::TEXT_VADDR, ctx));

    Ok(elf::write_elf_with_debug(
        &text,
        rodata.bytes(),
        state_size,
        0x3E, // EM_X86_64
        debug_sections.as_ref(),
    ))
}

/// Builds the four DWARF v5 sections from the finalized x86-64 program layout: one
/// `DW_TAG_subprogram` per lowered function (with a real, **file-aware** source
/// location) and one `.debug_line` sequence per function rebasing its surviving
/// fn-relative [`LineMark`]s to absolute `.text` addresses.
///
/// The `_start` shim and the runtime prelude have no source and are simply
/// omitted (an address in them resolves to `??`, which is honest). Functions are
/// indexed by `prog.funcs[id.index()]` to recover the display name + defining
/// line; only functions actually lowered into `.text` (the `lowered` vector) get
/// a DIE/sequence.
///
/// File awareness: the driver compiles one MERGED source (user file + appended
/// `std` prelude + any imported module), so a function/row line is a line of the
/// merged text. Each such line is translated back to its true `(file, line)` via
/// [`DebugCtx::source_map`]; the file becomes (or reuses) a line-table entry and
/// the line is the real source line. A line the map cannot place — or any line at
/// all when no map was supplied (a genuinely single-file build) — stays attributed
/// to the primary file (its referenceable index, see [`FileTable`]) at its raw
/// line, so single-file mapping is unchanged.
fn build_dwarf(
    prog: &MirProgram,
    lowered: &[LoweredFn],
    text_len: u64,
    text_vaddr: u64,
    ctx: &DebugCtx,
) -> elf::DebugSections {
    let mut funcs: Vec<dwarf::DwFn> = Vec::with_capacity(lowered.len());
    let mut lines: Vec<dwarf::DwSeq> = Vec::with_capacity(lowered.len());

    // The line-table file builder: index 0 is always the primary user source.
    let mut files = FileTable::new(basename(&ctx.src_path));

    for lf in lowered {
        let func = &prog.funcs[lf.id.index()];
        let low_pc = text_vaddr + lf.text_off as u64;
        let len = lf.code.len() as u64;

        // Resolve the function's defining line to its true (file, line). A function
        // whose merged line maps to no real source (synthesized scaffolding) keeps
        // the primary file at the raw line.
        let (decl_file, decl_line) = resolve_loc(&ctx.source_map, func.span.line, &mut files);
        funcs.push(dwarf::DwFn {
            name: func.name.clone(),
            low_pc,
            len,
            file: decl_file,
            decl_line,
        });

        // Build this function's line sequence: one row per surviving line mark
        // (rebased to an absolute address) plus a terminating end_sequence row at
        // the function's one-past-the-last byte. A function with no line marks is
        // given a single row at its `fn` line so a breakpoint still resolves. Each
        // row carries its OWN (file, line): a single function can contain code
        // inlined from more than one source file (a user `test` that inlines a `std`
        // helper), so resolving per row — not once for the function — is what keeps
        // an inlined std row in `std.k2` instead of pointing at a nonexistent line
        // of the user file. The line program emits a `DW_LNS_set_file` whenever the
        // file changes between consecutive rows.
        let mut rows: Vec<dwarf::DwRow> = Vec::with_capacity(lf.line_marks.len() + 2);
        if lf.line_marks.is_empty() {
            if decl_line != 0 {
                rows.push(dwarf::DwRow {
                    address: low_pc,
                    file: decl_file,
                    line: decl_line,
                    end_sequence: false,
                });
            }
        } else {
            for lm in &lf.line_marks {
                let (file, line) = match ctx.source_map.resolve(lm.line) {
                    Some((f, l)) => (files.intern(f), l),
                    None => (files.primary_index(), lm.line),
                };
                rows.push(dwarf::DwRow {
                    address: text_vaddr + (lf.text_off + lm.at) as u64,
                    file,
                    line,
                    end_sequence: false,
                });
            }
        }
        // Only emit a sequence when it has at least one real row (otherwise the
        // function had no source line at all — skip it cleanly). The end_sequence
        // row inherits the last row's file (it carries no real location).
        if !rows.is_empty() {
            let last_file = rows.last().map(|r| r.file).unwrap_or(decl_file);
            rows.push(dwarf::DwRow {
                address: low_pc + len,
                file: last_file,
                line: 0,
                end_sequence: true,
            });
            lines.push(dwarf::DwSeq { rows });
        }
    }

    let producer = format!("k2 v{}", env!("CARGO_PKG_VERSION"));
    dwarf::build(&dwarf::DwarfInput {
        producer: &producer,
        files: files.into_files(),
        comp_dir: &ctx.comp_dir,
        text_vaddr,
        text_len,
        funcs,
        lines,
    })
}

/// Translates a merged-source `line` to its true `(file_index, line)`: the
/// [`DwarfSourceMap`] recovers the originating file display name (interned into
/// `files`, deduplicated) and the real line. A line the map cannot place — or any
/// line when the map is empty (single-file build) — stays in the primary file (its
/// referenceable index, never 0) at its raw line.
fn resolve_loc(map: &DwarfSourceMap, line: u32, files: &mut FileTable) -> (u32, u32) {
    match map.resolve(line) {
        Some((file, real_line)) => (files.intern(file), real_line),
        None => (files.primary_index(), line),
    }
}

/// The line program's v5 file table under construction.
///
/// The layout is GNU-binutils-compatible: **file 0** and **file 1** are both the
/// primary user source, and each distinct `@import`-ed file is appended at index
/// **2+**. DWARF v5 makes index 0 the CU's primary (LLVM reads `DW_AT_name` from
/// it); but GNU `addr2line`/binutils still chokes on a line program that *references*
/// file index 0 ("bad file number"). So nothing the line program or `DW_AT_decl_file`
/// emits ever points at index 0: the primary is referenced as **index 1** (its
/// duplicate) and foreign files as their 2+ index. A single-file program therefore
/// reproduces the historical file0==file1 table exactly, and `addr2line` stays warning
/// -free under both GNU and LLVM. (`llvm-dwarfdump --verify` notes file 1 duplicates
/// file 0 — a benign warning, exit 0 — which is the price of GNU compatibility.)
struct FileTable {
    /// File display names in index order; `paths[0]` and `paths[1]` are the
    /// primary source (the GNU-compat duplicate), `paths[2..]` the imported files.
    paths: Vec<String>,
    /// `display name -> referenceable index`, so the same file is never added
    /// twice. The primary maps to 1 (not 0); foreign files to their 2+ index.
    index_of: HashMap<String, u32>,
}

impl FileTable {
    /// A fresh table whose files 0 and 1 are both `primary` (the user's source
    /// basename); the referenceable primary index is 1.
    fn new(primary: &str) -> FileTable {
        let name = basename(primary).to_string();
        let mut index_of = HashMap::new();
        index_of.insert(name.clone(), 1u32);
        FileTable {
            paths: vec![name.clone(), name],
            index_of,
        }
    }

    /// Returns the referenceable index of `display`, appending a new entry the
    /// first time it is seen. The display string is reduced to its basename so
    /// `addr2line` reports `std.k2` rather than a long path, matching the
    /// primary-file convention. The returned index is never 0 (see the type docs).
    fn intern(&mut self, display: &str) -> u32 {
        let name = basename(display);
        if let Some(&idx) = self.index_of.get(name) {
            return idx;
        }
        let idx = self.paths.len() as u32;
        self.paths.push(name.to_string());
        self.index_of.insert(name.to_string(), idx);
        idx
    }

    /// The referenceable index of the primary user source (always 1).
    fn primary_index(&self) -> u32 {
        1
    }

    /// Finalizes the table into the `dwarf` crate's file entries (all under
    /// directory 0, the single `comp_dir`).
    fn into_files(self) -> Vec<dwarf::DwFile> {
        self.paths
            .into_iter()
            .map(|path| dwarf::DwFile { path, dir_index: 0 })
            .collect()
    }
}

/// The final path component of `path` (its basename), or the whole string when it
/// has no separator. Used for the CU/line-table source file name so `addr2line`
/// reports `hello.k2` rather than a long path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `true` if a fixup targets the runtime prelude or the writable state segment
/// (the signal that this program needs the `*System` runtime emitted).
fn is_runtime_or_state(kind: FixupKind) -> bool {
    matches!(kind, FixupKind::Runtime(_) | FixupKind::State(_))
}

/// Compiles a [`MirProgram`] to a relocatable `ET_REL` x86-64 object (v0.19 C
/// interop): the stitched `.text` (no `_start` shim — crt0/libc provides it), the
/// `.rodata` blob, a `.symtab` with `extern` callees UNDEFINED and `export`/`main`
/// functions defined-GLOBAL, and a `.rela.text` of the call/data relocations. The
/// object is meant to be linked with the system `cc` (`-no-pie`) against libc.
///
/// This is a **parallel** entry point to [`compile_program`]; the freestanding
/// ELF path is untouched. A program that needs the `*System` runtime (heap /
/// capabilities) is refused here — the libc/FFI path does not emit the runtime — so
/// it stays on the directly-runnable freestanding path or the VM.
pub fn compile_program_to_object(
    prog: &MirProgram,
    target: Target,
) -> Result<ObjectImage, CodegenError> {
    if target != Target::X86_64Linux {
        return Err(CodegenError::Unsupported(format!(
            "C-interop object emission is x86-64-only; `{}` is not supported",
            target.triple()
        )));
    }

    // ---- Lower every defined function; an `extern` decl emits no body. ----
    let mut rodata = RoData::new();
    let mut lowered: Vec<LoweredFn> = Vec::new();
    for func in &prog.funcs {
        if func.is_extern_decl {
            continue; // an undefined external symbol — no code.
        }
        let (code, fixups) = FnLower::new(prog, func).lower(&mut rodata)?;
        // The FFI/libc path does not emit the `*System` runtime; a heap/capability
        // program must use the freestanding path or the VM instead.
        if fixups.iter().any(|f| is_runtime_or_state(f.kind)) {
            return Err(CodegenError::Unsupported(
                "a program using the `*System` runtime (heap / capabilities) cannot be linked \
                 with libc; build it with the freestanding `build-native` path or run it on the VM"
                    .into(),
            ));
        }
        lowered.push(LoweredFn {
            id: func.id,
            code,
            fixups,
            line_marks: Vec::new(),
            text_off: 0,
        });
    }

    // ---- Stitch the functions into `.text`, recording each one's offset. ----
    let mut text: Vec<u8> = Vec::new();
    let mut fn_offsets: HashMap<FnId, usize> = HashMap::new();
    let mut fn_sizes: HashMap<FnId, usize> = HashMap::new();
    for lf in &mut lowered {
        lf.text_off = text.len();
        fn_offsets.insert(lf.id, lf.text_off);
        fn_sizes.insert(lf.id, lf.code.len());
        text.extend_from_slice(&lf.code);
    }

    // ---- Build the symbol table: section symbols (local), defined fn globals,
    //      and the undefined extern callees. ----
    // Symbol index 0 is the null symbol; then the two LOCAL section symbols
    // (`.text` and `.rodata`, the relocation targets for data refs and intra-object
    // calls); then the GLOBAL defined functions and undefined externs.
    let mut symbols: Vec<ObjSymbol> = Vec::new();
    symbols.push(ObjSymbol {
        name: String::new(),
        bind: obj::abi::STB_LOCAL,
        typ: obj::abi::STT_NOTYPE,
        shndx: obj::abi::SHN_UNDEF,
        value: 0,
        size: 0,
    });
    // `.text` section symbol (index 1).
    let sym_text = symbols.len() as u32;
    symbols.push(ObjSymbol {
        name: String::new(),
        bind: obj::abi::STB_LOCAL,
        typ: obj::abi::STT_SECTION,
        shndx: obj::abi::SEC_TEXT,
        value: 0,
        size: 0,
    });
    // `.rodata` section symbol (index 2).
    let sym_rodata = symbols.len() as u32;
    symbols.push(ObjSymbol {
        name: String::new(),
        bind: obj::abi::STB_LOCAL,
        typ: obj::abi::STT_SECTION,
        shndx: obj::abi::SEC_RODATA,
        value: 0,
        size: 0,
    });

    // Defined GLOBAL function symbols. The entry `main` is renamed to the C symbol
    // `main` (so crt0 calls it); an `export fn` keeps its un-mangled C name. Other
    // helpers are emitted as GLOBAL `STT_FUNC` under their lowered name with a
    // sanitized, collision-free symbol (a monomorphized name like `List(u32).push`
    // is not a legal C identifier, but only `main`/exports are *referenced* by C,
    // so the helpers' names only matter for debugging — sanitize them).
    let main_id = find_main(prog);
    let mut sym_of_fn: HashMap<FnId, u32> = HashMap::new();
    let mut exports: Vec<String> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for lf in &lowered {
        let func = &prog.funcs[lf.id.index()];
        let (sym_name, is_global) = match &func.linkage {
            Linkage::ExportC(name) => {
                exports.push(name.clone());
                (name.clone(), true)
            }
            _ if Some(lf.id) == main_id => {
                exports.push("main".to_string());
                ("main".to_string(), true)
            }
            _ => (
                unique_symbol(&sanitize_symbol(&func.name), &used_names),
                false,
            ),
        };
        used_names.insert(sym_name.clone());
        let idx = symbols.len() as u32;
        sym_of_fn.insert(lf.id, idx);
        symbols.push(ObjSymbol {
            name: sym_name,
            bind: if is_global {
                obj::abi::STB_GLOBAL
            } else {
                obj::abi::STB_LOCAL
            },
            typ: obj::abi::STT_FUNC,
            shndx: obj::abi::SEC_TEXT,
            value: lf.text_off as u64,
            size: *fn_sizes.get(&lf.id).unwrap_or(&0) as u64,
        });
    }

    // ELF requires every LOCAL symbol to precede every GLOBAL one. The defined-fn
    // loop above interleaves local helpers and global exports, so partition the
    // symbol table into [null, locals..., globals...] and rebuild the fn->index map.
    let (symbols, sym_text, sym_rodata, sym_of_fn) =
        reorder_locals_first(symbols, sym_text, sym_rodata, sym_of_fn);

    // Undefined extern symbols (one per `extern` decl that some call references).
    let mut sym_of_extern: HashMap<FnId, u32> = HashMap::new();
    let mut had_extern = false;
    let mut symbols = symbols;
    for func in &prog.funcs {
        if !func.is_extern_decl {
            continue;
        }
        let name = match &func.linkage {
            Linkage::ExternC(n) => n.clone(),
            _ => sanitize_symbol(&func.name),
        };
        had_extern = true;
        let idx = symbols.len() as u32;
        sym_of_extern.insert(func.id, idx);
        symbols.push(ObjSymbol {
            name,
            bind: obj::abi::STB_GLOBAL,
            typ: obj::abi::STT_NOTYPE,
            shndx: obj::abi::SHN_UNDEF,
            value: 0,
            size: 0,
        });
    }

    // ---- Build `.text` relocations from the surviving fixups. ----
    let mut relocs: Vec<ObjReloc> = Vec::new();
    for lf in &lowered {
        for fx in &lf.fixups {
            let site = (lf.text_off + fx.at) as u64;
            match fx.kind {
                FixupKind::Call(callee) => {
                    if let Some(&sym) = sym_of_extern.get(&callee) {
                        // A call into libc (or another object): PLT32, addend -4.
                        relocs.push(ObjReloc {
                            offset: site,
                            sym,
                            typ: obj::abi::R_X86_64_PLT32,
                            addend: -4,
                        });
                    } else if let Some(&off) = fn_offsets.get(&callee) {
                        // An intra-object call: PLT32 against `.text`, addend
                        // `callee_off - 4` (the linker relaxes it to a direct
                        // PC32 since the target is local).
                        relocs.push(ObjReloc {
                            offset: site,
                            sym: sym_text,
                            typ: obj::abi::R_X86_64_PLT32,
                            addend: off as i64 - 4,
                        });
                    } else {
                        return Err(CodegenError::Unsupported(
                            "call to an unknown fn in object emission".into(),
                        ));
                    }
                }
                FixupKind::Data(off) => {
                    // A `.rodata` pointer (`mov r64, imm64`): absolute 64-bit
                    // relocation against the `.rodata` section symbol (needs a
                    // non-PIE link, `cc -no-pie`).
                    relocs.push(ObjReloc {
                        offset: site,
                        sym: sym_rodata,
                        typ: obj::abi::R_X86_64_64,
                        addend: off as i64,
                    });
                }
                FixupKind::Runtime(_) | FixupKind::State(_) => {
                    // Rejected above; never reached.
                    return Err(CodegenError::Unsupported(
                        "runtime reference in a libc-linked object".into(),
                    ));
                }
                FixupKind::Local(_) => {
                    // Resolved by `Asm::finish`; should not survive.
                }
            }
        }
    }

    let _ = sym_of_fn; // defined-fn symbols aid debugging; calls go via `.text`.
    Ok(obj::write_object(
        &text,
        rodata.bytes(),
        &symbols,
        &relocs,
        had_extern,
        exports,
    ))
}

/// Partitions a symbol table built with interleaved locals/globals into
/// `[null, all-locals..., all-globals...]` (an ELF ordering requirement) and
/// rewrites the section-symbol indices and the fn->symbol map to the new order.
fn reorder_locals_first(
    symbols: Vec<ObjSymbol>,
    sym_text: u32,
    sym_rodata: u32,
    sym_of_fn: HashMap<FnId, u32>,
) -> (Vec<ObjSymbol>, u32, u32, HashMap<FnId, u32>) {
    // Build the new order: index 0 stays null; locals (non-null) first, then
    // globals, preserving relative order within each class.
    let mut order: Vec<usize> = vec![0];
    for (i, s) in symbols.iter().enumerate().skip(1) {
        if s.bind == obj::abi::STB_LOCAL {
            order.push(i);
        }
    }
    for (i, s) in symbols.iter().enumerate().skip(1) {
        if s.bind == obj::abi::STB_GLOBAL {
            order.push(i);
        }
    }
    // old index -> new index.
    let mut remap = vec![0u32; symbols.len()];
    for (new_idx, &old_idx) in order.iter().enumerate() {
        remap[old_idx] = new_idx as u32;
    }
    let new_symbols: Vec<ObjSymbol> = order.iter().map(|&i| symbols[i].clone()).collect();
    let new_fn_map: HashMap<FnId, u32> = sym_of_fn
        .into_iter()
        .map(|(k, v)| (k, remap[v as usize]))
        .collect();
    (
        new_symbols,
        remap[sym_text as usize],
        remap[sym_rodata as usize],
        new_fn_map,
    )
}

/// Sanitizes a lowered fn name into a valid (best-effort) symbol identifier: any
/// non-`[A-Za-z0-9_]` byte becomes `_`. Only `main`/exports are *referenced* by C;
/// helper names only need to be valid + collision-free (handled by
/// [`unique_symbol`]) for the symbol table and `readelf`/`objdump`.
fn sanitize_symbol(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Returns `base`, or `base_2`/`base_3`/… if `base` is already taken, so two
/// distinct helpers never share a symbol name.
fn unique_symbol(base: &str, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}_{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
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
