# DWARF debug info in the native backend (v0.27)

The freestanding x86-64 backend can emit **DWARF v5 debug info** into the static
ELF it produces, so a debugger can map machine addresses back to k2 source
locations and show k2 function names in a backtrace. Pass `-g` / `--debug-info`
to `k2c build-native` / `k2c run-native`; DWARF is **on by default in `--debug`
mode** and **off in `--release-fast` / `--release-safe`** (override either way with
`-g` or `--no-debug-info`).

```sh
k2c build-native -g examples/hello.k2 -o /tmp/h
llvm-dwarfdump --verify /tmp/h     # → "No errors."
addr2line -f -e /tmp/h 0x401043    # → main  /…/hello.k2:27
/tmp/h                             # still prints the expected output
```

## What is emitted

DWARF lives in **non-loaded** ELF sections (`SHT_PROGBITS` with `sh_flags = 0`,
`sh_addr = 0`), appended after the loaded image together with a section-header
table. The four sections are:

| section | contents |
|---------|----------|
| `.debug_abbrev` | the abbreviation table: a `DW_TAG_compile_unit` (with children) and a `DW_TAG_subprogram` (leaf). |
| `.debug_str` | a `\0`-led string pool referenced by `DW_FORM_strp` (producer, source name, comp_dir, each function name). |
| `.debug_line` | a v5 line-number program — **one sequence per function** — mapping each code-address range to its `(file, line)`. The file table is **file-aware**: one entry per distinct source file (the user's file plus any `@import`-ed module such as the bundled `std`), and each row selects its file with `DW_LNS_set_file`, so an inlined std function's addresses resolve to *its* file at *its* real line. |
| `.debug_info` | the compilation-unit DIE (`producer`, `name`, `comp_dir`, `low_pc`/`high_pc` over `.text`, `stmt_list`) and one `DW_TAG_subprogram` DIE per emitted function (`name`, `low_pc`, `high_pc`, `decl_file`, `decl_line` — `decl_file` carries that function's own file index). |

Plus an ELF **section-header table** with a NULL section, `.text` and `.rodata`
(carrying their real load `sh_addr`, so `objdump`/`addr2line` resolve addresses),
the four `.debug_*` sections, and `.shstrtab`.

### DWARF flavor and tool compatibility

* **DWARF v5, DWARF32, 8-byte addresses.** v5 makes the primary source file index
  0 (the CU's `DW_AT_name`), and `addr2line` reports the basename directly.
* `DW_AT_high_pc` uses the modern **offset form** (`DW_FORM_data8`,
  `high_pc = low_pc + value`), which LLVM and `addr2line` both expect.
* **File-aware, GNU-compatible file table.** The driver compiles a single *merged*
  source — the user's file with the `std` prelude appended (and, on the VM path,
  any `@import`-ed module) — so a function's raw span line is a line of that merged
  text, which for imported code is *not* a line of the user's file. A line map
  recovers each merged line's true `(file, line)`; the line program emits one file
  entry per distinct source file and a `DW_LNS_set_file` per row, and each
  subprogram's `DW_AT_decl_file` carries its own file. The primary source is still
  declared as **both file 0 and file 1** (identical) and imported files follow at
  index 2+; nothing the line program or `DW_AT_decl_file` references ever points at
  index 0, because GNU `addr2line`/binutils rejects file index 0 in the line
  program ("bad file number"). The primary is referenced as **1**, imports as 2+.
  This keeps **both** GNU `addr2line` and LLVM tools warning-free and resolving
  correctly. `llvm-dwarfdump --verify` notes that file 1 duplicates file 0 — a
  benign warning kept deliberately for GNU compatibility; verify still exits 0 with
  **No errors**. Before this fix (v0.27 initial), the emitter assumed a single file
  and mis-attributed every inlined `@import`-ed function to the main file's
  *nonexistent* lines; it is now file-aware.

## The hard guarantee: DWARF never changes what runs

DWARF is pure trailing metadata. The loaded image — the ELF header's loaded
fields, the program headers, `.text`, `.rodata`, and the writable state segment —
is **byte-for-byte identical** whether or not `-g` is set; only four section-table
fields in the ELF header (`e_shoff`, `e_shentsize`, `e_shnum`, `e_shstrndx`, all
zero in a `-g`-off build) change, and the `.debug_*` bytes are appended past the
last `PT_LOAD`. The kernel maps and runs the identical bytes.

This is enforced at three levels:

1. `Asm::set_line` only appends to a side-channel; it emits **no machine bytes**.
   Line marks ride through the peephole exactly like fixups (bucketed into
   instructions, rebased on re-serialization), so a moved or deleted instruction
   never desyncs the line table and never changes `.text`.
2. `elf::write_elf_with_debug(.., None)` is the historical writer verbatim — the
   `-g`-off output is byte-identical to v0.26 (asserted by a golden test).
3. Integration tests build the same program with and without `-g`, byte-compare
   the loaded region, and assert identical stdout + exit code.

`k2c build-native --no-debug-info examples/hello.k2` reproduces the exact v0.26
8520-byte `hello` binary.

## Validation (the oracle)

There is **no gdb/lldb in this environment**, so the DWARF is validated with the
LLVM/binutils tooling that debuggers' readers are built on:

* `readelf -S` — confirms the `.debug_*` sections exist.
* `llvm-dwarfdump --verify` — parses every section and cross-checks; must report
  **No errors.**
* `llvm-dwarfdump --debug-info` / `--debug-line` — confirms the CU, the per-
  function subprograms (with correct `low_pc`/`high_pc` and names), and the line
  table.
* `addr2line -e <bin> <addr>` — maps an in-function address to the right
  `<src>:<line>` (and `-f` reports the k2 function name).

Because these tools consume the **same** DWARF a debugger does, live source-level
debugging — breakpoints, `step` by line, and a backtrace showing k2 function
names + `file:line` — is **expected to work under gdb/lldb on a host that has
them**. The always-on byte-level encoder tests (`crate::tests::dwarf`,
`dwarf::unit`) and the tool-gated end-to-end tests (`tests/dwarf_native.rs`) pin
both the encoding and the oracle results.

## Scope (v0.27)

* **x86-64 freestanding only.** The aarch64 cross-compile path and the
  `--link-libc` C-interop path ignore `-g` this milestone (the binary is still
  produced, just without `.debug_*`).
* **Line table + compile-unit + per-function subprograms** are the shipped core.
  The `_start` shim and the `*System` runtime prelude have no source location and
  are intentionally omitted (an address inside them resolves to `??`, which is
  honest rather than misleading).
* **Multi-file / `@import`-ed code is file-aware** (the v0.27 follow-up fix): an
  inlined `std` (or other imported) function resolves to *its* file at *its* real
  line, never to a nonexistent line of the user's main file. See the file-table
  note under *DWARF flavor*.
* **Locals / parameter / base-type DIEs** (`DW_TAG_variable` / `DW_TAG_base_type`
  with a frame-base location) are a stretch goal not shipped in v0.27; they are
  purely additive DIEs that never affect `.text`, and can be added without
  touching the line/subprogram core.
