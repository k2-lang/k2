# Changelog

All notable changes to k2 are recorded here. The format is loosely based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it
reaches `0.1.0`.

While the version is `0.0.x`, **anything may change at any time** — the language
is being designed in the open and nothing is stable yet.

## [Unreleased]

### Fixed

- **Multi-file DWARF mis-attributed `@import`-ed code to the main file's
  nonexistent lines (v0.27 follow-up).** The native backend compiles a *merged*
  source (the user's file with the `std` prelude appended), so an inlined std
  function's span line is a line of that merged text — beyond the end of the
  user's file. The initial v0.27 emitter assumed a single source file and emitted
  a hardcoded `DW_AT_decl_file` / a one-entry line-table file, so e.g.
  `expectEqual[testing]` resolved to `errors.k2:1373` (a line that does not exist
  in the 184-line `errors.k2`). The emitter is now **file-aware**: a merged-line →
  true-`(file, line)` map (mirroring the v0.24 multi-file `SourceMap`) is threaded
  through `DebugCtx`, the line program emits one file entry per distinct source
  file and a per-row `DW_LNS_set_file`, and each `DW_TAG_subprogram`'s
  `DW_AT_decl_file` carries its own file. An inlined std address now resolves to
  `std.k2` at its real line; user addresses still map to the correct user
  `(file, line)`; single-file mapping is unchanged; `llvm-dwarfdump --verify`
  stays clean and both GNU and LLVM `addr2line` resolve without a "bad file
  number" error. Resolving per *row* (not per function) is what keeps a `std`
  statement inlined into a user `test` body in `std.k2` rather than at a
  nonexistent user-file line. Regression tests assert the file table is
  well-formed and that no address resolves to a line beyond the user file's
  length.

### Added

- **`k2c doc` documentation generator + doc-tests (v0.28).** A new `doc`
  subcommand extracts the `///` doc comments attached to public declarations
  (`pub fn`/`pub const`/`pub var`, and `pub` `struct`/`enum`/`union` types with
  their `pub` fields/members) and renders a **self-contained, dependency-free**
  HTML site — an index page plus per-module pages, with anchors and intra-doc
  cross-links — driven entirely by pure-`std` string building (no external CSS/JS,
  inline stylesheet, all content HTML-escaped). Each item emits its **signature
  pulled from the type checker** (`fn` params `name: type` + return type;
  struct/enum/union fields with types; const/var types — e.g. the resolved
  `error{Empty,NotANumber,OutOfMemory,Overflow}!*u32`), with parameter *names* from
  the AST, falling back to the AST type expressions only when a file fails to
  type-check (so the generator never panics on any parseable input). The
  doc-comment Markdown (headings, code spans/blocks, lists, links) renders to HTML
  through a small, total CommonMark subset; a `[x](javascript:…)` link is
  neutralized. `--format=html|md|both` adds a Markdown site; a directory argument
  documents every `*.k2` with a linking top-level index.

  **Doc-tests:** fenced ```` ```k2 ```` blocks in doc comments are extracted,
  compiled, and run as real `test` blocks via the existing VM harness under
  `Debug` (safety checks + the leak-checking allocator stay live), so an example
  that traps, mis-asserts, leaks, or lets an error escape **FAILS**; a
  `compile_fail` example passes iff it does not compile (a non-compiling example is
  reported as a doc-test *failure*, never a crash); `no_run` compiles without
  executing; `ignore`/foreign-language fences are skipped. Each example compiles in
  the context of its file (leading `const … = @import(…)` imports are hoisted to
  file scope) so it can reference the file's items. Wired as `k2c doc --test`
  (embeds pass/fail badges into the HTML and gates the exit code) and
  `k2c test --doc <file>` (run only the doc examples); both exit nonzero on any
  doc-test failure.

- **DWARF v5 debug info in the native x86-64 backend (v0.27).** `k2c
  build-native -g` / `run-native -g` (default ON in `--debug`, OFF in
  `--release-*`; `--no-debug-info` opts out) emit a real ELF **section-header
  table** plus `.debug_abbrev` / `.debug_str` / `.debug_line` / `.debug_info`
  sections, so a debugger maps machine addresses to k2 source locations and shows
  k2 function names. The `.debug_info` carries a `DW_TAG_compile_unit`
  (producer / source name / comp_dir / `low_pc`/`high_pc` over `.text` /
  `stmt_list`) and one `DW_TAG_subprogram` per emitted function (name, `low_pc`,
  `high_pc`, `decl_file`, `decl_line`); the `.debug_line` program maps each
  machine-code range to its `(file, line)` from the MIR per-statement spans, one
  sequence per function. Hand-emitted in pure `std` (no `gimli`/`object`),
  validated by `llvm-dwarfdump --verify` (zero errors), `addr2line` (in-`main`
  address → right `hello.k2` line + function name), and `readelf -S`.
  - The DWARF is **pure unmapped metadata**: the loaded image — program headers,
    `.text`, `.rodata`, the state segment — is byte-for-byte identical with and
    without `-g` (only four section-table header fields differ), so DWARF never
    changes what runs. `build-native --no-debug-info hello.k2` reproduces the
    exact prior 8520-byte binary. Line marks ride through the machine peephole
    like fixups, so a moved/deleted instruction never desyncs the table.
  - Scope: x86-64 freestanding only this milestone; aarch64 and `--link-libc`
    ignore `-g` (binary still produced). Local-variable / base-type DIEs are
    deferred (additive, never touch `.text`). See `docs/dwarf.md`. No gdb here;
    live debugging is expected to work under gdb/lldb on a host that has them,
    consuming the same DWARF the oracle tools validate.

- **OS / IO / net / time capabilities through `*System` (v0.23).** Real OS
  effects, every one a capability *value* reached only through the root
  `*System` (no ambient global). The VM backs them with Rust `std` (`std::fs`,
  `std::net`, `std::time`, `std::env`, `std::process`); the native backend
  implements the feasible subset with raw Linux syscalls and **cleanly refuses**
  the rest (`CodegenError::Unsupported` → "run it on the VM"), never a
  miscompile. All tests use TEMP files + LOOPBACK only, are self-cleaning, and
  assert only inequalities for real time — deterministic and offline.
  - **`sys.fs` / `std.fs`.** `openRead`/`create`/`openReadWrite` (→ `File`),
    `stat`/`exists`/`delete`, `makeDir`/`removeDir`, `listDir(alloc, path)`; a
    `File` does `read`/`write`/`stat`/`close`. Errors are an honest `FsError`
    set mapped from the host's `io::ErrorKind`. A program writes a temp file and
    reads back the IDENTICAL contents with the correct stat size, then deletes
    it. (VM via `std::fs`; native cleanly refuses fs.)
  - **`sys.os` / `std.os` + `sys.env`.** `argCount()`/`arg(i)`/`args(alloc)`
    read the forwarded argv (everything after `--`, threaded by `k2c run`/
    `run-native`); `getpid()` and `exit(code)`; `sys.env.get(name)` returns
    `?[]const u8`. Reproducible by default: env is offline-absent (host env only
    with `--real-env`; a scripted var with `--env=KEY=VALUE`) and `getpid()` is a
    deterministic `1` (real with `--real-pid`). Native implements `getpid`/`exit`
    as raw syscalls; `args(alloc)`/`env.get` are VM-only (cleanly refused).
  - **`sys.time` / `std.time`.** Real `monotonicReal()` (non-decreasing),
    `nowReal()` (wall Unix nanos), and `sleepReal(ns)` (a real delay), ALONGSIDE
    the unchanged deterministic `sys.clock`. The pure-k2 `Duration`/`Instant`
    value types (`fromMillis`/`asMillis`, `fromNanos`/`elapsedSince`) work over
    either clock. Real time is opt-in per call, so it never perturbs a
    deterministic run.
  - **`sys.net` / `std.net`.** TCP over loopback: `listen(port)` (port 0 =
    ephemeral, read back with `localPort()`), `connect(host, port)`, a
    `TcpListener.accept()` → `TcpStream` with `send`/`recv`/`close`. A
    single-fiber loopback echo round-trips bytes correctly. (VM via `std::net`;
    native cleanly refuses net.)
  - Plumbing: `RunArgs` now threads the forwarded argv and an `OsInputs` (scripted
    env + real-env/real-pid opt-ins) into the VM; `k2c run` gains `--env=K=V`,
    `--real-env`, `--real-pid` and forwards `-- argv...` to the program (the VM
    and the native child both). The v0.23 fs/net error names are pre-seeded with
    stable tags (like `OutOfMemory`/`NoSpaceLeft`) so `@errorName`/`catch` name
    them even though the door synthesizes them in the VM.

- **Stdlib data structures (v0.22).** The bundled `std` (written in k2, in
  `crates/k2-std/std/std.k2`) gains a family of containers, algorithms, and
  allocators, each exercised by a running program (VM is the semantic reference;
  native where it compiles, else cleanly refused):
  - **`std.HashMap(K, V, Context)`** — a generic, allocator-taking hash map using
    OPEN ADDRESSING with linear probing, tombstone deletion, and dynamic RESIZE at
    a 75% used+tombstone load factor (power-of-two capacity, cheap mask indexing).
    `put`/`get`/`getPtr`/`getOrPut`/`contains`/`remove`/`count`/`iterator` with a
    nested `Entry`/`Iterator`/`GetOrPutResult`. Hash/eq are a comptime `Context`
    type (a function value cannot be passed): `IntContext(K)` (overflow-free
    Fibonacci/Knuth multiplicative hash) and `StrContext` (FNV-1a folded to 32
    bits). `IntHashMap`/`StringHashMap`/`AutoHashMap` are the thin wrappers.
    Verified: 1000 inserts across several grows, full readback, update, remove-
    evens + tombstone reinsertion, iterate — all correct and leak-clean.
  - **`std.sort`** — `Sorter(T, Ctx).sort(slice)` sorts in place (introsort-lite:
    Hoare quicksort with an insertion-sort cutoff at n<16), plus `insertionSort`,
    `isSorted`, and `binarySearch`. Order is a comptime `Ctx` with `lessThan`;
    `sort.asc(T)`/`sort.desc(T)` build them. Ascending AND descending in one
    program is correct.
  - **`std.unicode`** — UTF-8 `utf8Len`/`utf8DecodeAt`/`utf8Validate`/
    `utf8CountCodepoints`/`utf8Encode` (out-buffer `*[4]u8`), correct on ASCII,
    2/3/4-byte sequences, and rejected invalid/truncated input. Plus `std.ascii`
    single-byte classification/case.
  - **`std.math` + `std.Big`** — `min`/`max`/`clamp`/`absI64`/`gcd`/`lcm`/`powU64`
    (all overflow-free), and a fixed-width 256-bit big integer (`add`/`sub`/`mul`/
    `cmp`/`toDecimal`) over eight little-endian u32 limbs.
  - **New allocators** — a `CountingAllocator` wrapper that tallies alloc/free/
    bytes while forwarding to an inner `Allocator` (the inner GPA still leak-checks
    clean), and a `StackAllocator` (bump-over-a-buffer alias of the
    `FixedBufferAllocator`).
- **Compiler fixes enabling the above (all minimal, all behind the existing
  green suite):**
  - **Per-instantiation member resolution** (`k2-types`): a generic method body's
    member dispatch on a comptime-TYPE param (`Context.lessThan` inside
    `Sorter(T, Asc)` vs `Sorter(T, Desc)`) is now recorded under the enclosing
    instantiated struct type, and the MIR keys private sibling-helper calls
    (`sort` → `quick` → `insertionRange`) by that same instantiation — so two
    contexts in one program no longer collapse to a single (last-checked) target.
    Member resolution is also order-independent (a concrete target is never
    downgraded to `Deferred` by a later static check).
  - **Heap-backed byte-slice formatting** (`k2-vm`): a `[]const u8`/`[]u8` built at
    run time (e.g. `Big.toDecimal`'s digit run, a `buf[0..n]` view) now renders
    correctly under `{s}`/`{}` — the format path materializes a byte slice's heap
    bytes before the (heap-blind) format engine runs.
  - **Native clean-refusal of unresolved-element slice indexing** (`k2-codegen`): a
    bare slice whose element type is still `deferred` (an un-monomorphized generic
    helper param) is refused with the standard "run it on the VM" note instead of
    mis-striding at the word size and producing wrong results.

- **Rich diagnostics & error-return traces (v0.20).** Every phase can now attach a
  *primary labelled span* plus zero-or-more *secondary labelled spans*, *notes*,
  and a *help/suggestion* to a diagnostic, and the driver renders them in a
  rustc/ariadne-style report — pure std, zero external crates. Components:
  - **A shared rich model** (`k2_syntax::{RichDiagnostic, Label, RichSeverity}`).
    Each phase's `Diagnostic` gains additive `primary_label`/`labels`/`notes`/
    `help` fields (default-empty, so every existing constructor and `.message`/
    `.span` assertion is unchanged) and a `to_rich()` conversion.
  - **A pure-std caret renderer** (`k2c::render`) that prints the
    `severity: message` header, a `--> file:line:col` locator, the source line(s)
    with a line-number gutter, a `^^^` underline under the primary span (with its
    inline label), secondary `---` underlines, multi-line span rails, and
    `note:`/`help:` lines. It aligns the caret by **display column** — multi-byte
    UTF-8 counts the right cells, CJK/emoji are width-2, combining marks are
    width-0, and tabs are reproduced verbatim so alignment holds at any terminal
    tab width. It honours `NO_COLOR`/`K2_NO_COLOR`/`K2_COLOR` and only colours a
    tty, and it **never panics** on any input (empty file, EOF/past-EOF spans,
    zero-width spans, 100 000-char lines, tab-only lines).
  - **Wired into every `k2c` subcommand** (parse/ast/fmt/resolve/check/mir/run/
    build/…), replacing ~12 hand-rolled one-line formatters with one path;
    multiple diagnostics print in source order.
  - **Upgraded high-value diagnostics:** type mismatch (primary "this is `T`" +
    an `@as` help when both sides are numeric), undeclared name (primary "not
    found in this scope" + a Levenshtein "did you mean `x`?" help), duplicate /
    shadow decl (primary on the redeclaration + a secondary on the original),
    non-exhaustive switch (note listing the missing cases + a help), and
    parse-expected (a zero-width caret + a "while parsing …" note).
  - **Error-return traces (VM).** A `@errorReturnTrace()` builtin (opaque
    `?*StackTrace`, `null` for now) plus runtime instrumentation: each `try`
    that re-throws records its source site, and when an error escapes `main` in
    Debug/ReleaseSafe the runtime prints an `error return trace:` block listing
    those sites newest-first (Zig-style). In **ReleaseFast** the whole mechanism
    is stripped at compile time — no `ReturnErr` instruction, no per-fiber buffer,
    byte-identical hot path. Native error-return traces are deferred/best-effort
    (the shim ABI is specified for a later milestone); v0.20 ships them in the VM
    (`k2c run`), which is what the acceptance gate verifies.
- **C interop & FFI (v0.19): call libc from k2, expose k2 to C.** A k2 program can
  declare `extern fn puts(s: [*:0]const u8) c_int;`, call it, compile to a
  relocatable object, link with the system `cc`, and RUN with the right output;
  and an `export fn add(a: c_int, b: c_int) c_int { ... }` produces a stable,
  un-mangled C symbol a gcc-compiled `main` can call. Components:
  - **`c_*` integer aliases are concrete C-ABI widths.** `c_char`/`c_short`/
    `c_int`/`c_long`/`c_longlong` (+ unsigned) map to the LP64 widths
    (8/16/32/64/64), so `@sizeOf(c_int) == 4` etc. fall straight out of the shared
    layout math; `@sizeOf`/`@alignOf` of representative `extern struct`s match C
    `sizeof`/`_Alignof` (verified by compiling + running an equivalent C program).
    `c_longdouble` (the 80-bit x87 `long double`) is mapped to `f128` and rejected
    by the FFI gate rather than silently miscompiled.
  - **A many-item / sentinel pointer type `[*]T` / `[*:0]const u8`** — a raw
    eightbyte pointer usable as a C `T *` / `const char *`. A string literal passed
    to such a parameter decays to its data pointer (NUL-terminated), so
    `puts("hi")` marshals a `const char *`, not a fat `{ptr,len}` slice.
  - **`extern` / `export` typing.** An `extern fn` is a body-less undefined C
    symbol the program calls; an `export fn` is a defined global C symbol. Both are
    checked for FFI-representability (a slice/optional/error-union/non-`extern`
    struct-by-value parameter is rejected with a clear message); `...`-variadic
    externs (printf-class) are supported.
  - **A pure-std `ET_REL` ELF64 object writer** (`obj.rs`): `.text` + `.rodata` +
    a `.symtab` (externs UNDEFINED, `export`/`main` defined GLOBAL `STT_FUNC`) +
    `.rela.text` (`R_X86_64_PLT32` for an `extern` call, `R_X86_64_64` for a
    `.rodata` pointer). Hand-written, no external crates.
  - **System-linker integration.** New `k2c build-native --link-libc <file> -o out`
    (and the `run-native` equivalent) emit the object and link it into a **dynamic**
    executable by shelling out to the system `cc`/`gcc` (`-no-pie`) as the link
    driver — exactly as `rustc` invokes the platform linker; the compiler itself
    stays pure-std. A variadic call zeroes `AL` (the SysV vector-register count) so
    `printf` reads its arguments correctly. The FFI/link tests gate on `cc`
    presence and skip cleanly when absent.
  - The **freestanding native path is unchanged**: `hello`/`errors`/`allocators`
    still run with `native == VM`, and the ET_EXEC writer / `_start` shim / runtime
    are untouched (the object path is a parallel entry point).
- **A second native target: aarch64 (ARMv8-A) Linux + cross-compilation
  (v0.18).** `k2c build-native --target=aarch64-linux <file> -o out` cross-compiles
  hello-class k2 programs to a static, EM_AARCH64 ELF, alongside the original
  x86-64 backend. The same monomorphized MIR drives both targets. Components:
  - A **target abstraction** (`Target` enum + `SysNr` syscall table): the ELF
    `e_machine`, the per-arch Linux syscall numbers, and supported-triple parsing
    live in one place. The x86-64 path is preserved **bit-for-bit** — it is reached
    through `Target::X86_64Linux` (the default) with zero changes to its
    encoder/lowering/runtime, so `hello`/`errors`/`allocators` still run with
    `native == VM` and the speedup holds (verified).
  - A **fixed-32-bit-little-endian aarch64 instruction encoder**
    (`movz`/`movk`/`movn`, `add`/`sub`/`mul`/`sdiv`/`udiv`/`msub`, `and`/`orr`/
    `eor`/`mvn`/`neg`, register + immediate shifts, `cmp`/`subs`/`cset`, `ldr`/
    `str` in all four sizes signed+unsigned with `[fp,#off]` addressing, `stp`/
    `ldp` frame pairs, `b`/`b.cc`/`bl`/`ret`, `adrp`/`add`, `svc #0`, and the
    `fadd`/`fsub`/… scalar-double family), with **~45 byte-exact unit tests** each
    cross-checked against the ARM ARM (DDI 0487) encoding tables.
  - An **EM_AARCH64 ELF writer** (the shared layout writer parameterized by
    `e_machine`) and an aarch64 **AAPCS64 MIR lowering** covering the hello-class
    subset: the `stp x29,x30,[sp,#-16]!` frame, parameter receipt (`x0`–`x7`),
    scalar/compare/bitwise/shift arithmetic (width-correct via `ubfm`/`sbfm`), the
    `print` formatter (literals + `{s}`/`{d}`/`{}`/`{x}`/`{X}`/`{b}`/`{o}`/`{c}`,
    incl. 64- and 128-bit decimals via `msub`-remainder long division), the CFG
    terminators, the escaped-`main`-error path, the safety-check `Trap` lowering,
    and the `write`/`exit_group` syscalls. The `*System` heap runtime is **not yet
    ported** to aarch64: a program that needs it is refused with a clean
    `Unsupported` deferral (never a miscompile), matching how the x86 backend
    rejects out-of-subset constructs.
  - **HONESTY (verification constraint).** aarch64 binaries are **cross-compiled
    and structurally validated, never executed** in this environment — there is no
    `qemu-aarch64`, no aarch64 binutils, and the host `objdump` cannot disassemble
    aarch64. Correctness rests on the byte-exact encoder tests vs the published ARM
    ARM encodings, plus parsing the emitted ELF header (EM_AARCH64=183, ET_EXEC, a
    valid entry/PT_LOAD) and `readelf -h`/`file` confirming an ARM aarch64
    executable. The binaries are *expected* to run on real aarch64 Linux; that has
    **not** been demonstrated here. See `docs/aarch64.md`.

- **Native optimization + machine-level peephole + native-vs-VM benchmark
  (v0.17).** The MIR optimizer is now wired into the native pipeline:
  `k2c run-native`/`build-native` honor `--debug` (unopt, checks on),
  `--release-safe` (opt + checks kept), and `--release-fast` (opt + checks
  stripped at lowering) exactly like the VM path — the optimizer runs on the MIR
  *before* native lowering, and the native output is unchanged by optimization
  (differential: native-opt == native-unopt == VM, same stdout + exit, verified
  by running the emitted binaries). Wiring the optimizer in exposed a real
  miscompile that the old `OptLevel::None`-only native tests could not see —
  copy/const propagation folds a string constant inline into a print tuple
  (`Tuple { str#1, … }`), and the deferred-aggregate lowering mis-typed the bare
  `Const::Str` field as the surrounding tuple type and routed it to the scalar
  `const_to`, which rejected it (`non-scalar constant Str(..)`). Fixed by typing a
  string-constant aggregate field as the canonical `[]const u8` slice in both the
  lowering (`build_aggregate`) and the register allocator's synthetic layout
  (`operand_decl_type`), so it flows through the existing slice-const store path.
  - A **machine-level peephole pass** over the emitted instruction stream: the
    encoder records a lightweight `ITag` classification per instruction, and a
    fixpoint pass deletes redundant reg-reg self-moves, folds `mov r, 0` into
    `xor r, r` when the flags are provably dead, removes dead register stores,
    collapses jump-to-next (fall-through) branches, and threads jump-to-jump
    forwarding blocks — then re-serializes, re-deriving every fixup/label offset
    so deletion is just "drop an instruction". Unrecognized instructions are
    opaque barriers across which no rule reasons, making the pass
    behavior-preserving by construction (and verified differentially). It shrinks
    `.text` by ~1–2% on the runtime-heavy corpus (e.g. 361 bytes / 1.7% on
    `errors`) with byte-identical behavior.
  - A **native benchmark** (`k2c bench --native`, also appended to the default
    `k2c bench`) compiles the compute kernels to a native ReleaseFast ELF and
    measures **wall-clock** native (process exec, best-of-5) vs the VM (in-process,
    best-of-5) on the *same* optimized MIR, asserts their stdout/exit agree, and
    reports the speedup. The committed `bench/native_baseline.md` records the
    measured numbers; the CI gate is a non-flaky conservative `>= 5x` floor
    (`native_is_much_faster_than_vm`), with the real measured margin many times
    larger. ReleaseFast safety-check stripping stays correct: a `u8` overflow that
    traps in Debug native (exit 134) is absent in ReleaseFast native (wraps,
    exit 0), matching the VM in each mode.

- **Language design.** The full specification of k2 — *Kardashev Type II:
  total control over the machine, with zero waste* — a systems language that
  takes Zig's design philosophy (no hidden control flow, no hidden allocation,
  no ambient authority, `comptime` as the only metaprogramming, errors as
  values, native speed with no runtime and no GC) and implements its toolchain
  in Rust.
  - `docs/philosophy.md` — the design pillars and what k2 keeps, drops, and
    changes relative to its inspirations.
  - `docs/spec/01`–`10` — lexical structure, types, expressions and statements,
    functions, memory and allocators, error handling, `comptime`, modules and
    the build system, concurrency, and the standard library.
  - `docs/grammar.ebnf` — the complete reference grammar.
  - `docs/compiler-architecture.md` — the planned pipeline and the dual
    Cranelift (debug) / LLVM (release) backend strategy.
  - `docs/tooling.md` — the `k2c` driver, `build.k2`, and the formatter.
  - `examples/` — runnable `.k2` programs covering hello-world, allocators,
    error handling, `comptime` reflection, generics, and a `build.k2`.

- **Toolchain front-end (Rust).** A Cargo workspace using only the standard
  library, so it builds and tests fully offline.
  - `k2-lexer` — a complete, recovering lexer for the surface syntax, with an
    extensive unit-test suite.
  - `k2-syntax` — the AST type definitions and source-span machinery.
  - `k2-vm` — the v0.8 bytecode compiler + register VM + runtime shim: it
    compiles the monomorphized MIR to a compact register ISA and executes
    `main(sys)` on a managed heap, with the minimal io/heap capability
    intrinsics (`sys.io.stdout`/`stderr`, `Writer.print`, `sys.heap` with
    `create`/`destroy`/`alloc`/`free`). A failed safety check / `Trap` /
    `unreachable` becomes a clean runtime panic (a `panic:` line on stderr and
    a nonzero exit), never an uncontrolled Rust panic; `defer`/`errdefer`
    ordering and `try` error-propagation execute straight from the CFG.
  - `k2c` — the compiler driver, with a working `tokenize` / `lex` subcommand
    that streams tokens from a file or standard input, plus the `run`
    subcommand that compiles and executes a program (Debug or `--release-fast`).
  - `k2-opt` — the v0.9 MIR optimizer: a pass pipeline run to a fixpoint
    (constant folding, constant/copy propagation, dead-code/dead-store
    elimination, CFG simplification, small-monomorphic-call inlining /
    devirtualization with size + recursion budgets, and — in ReleaseSafe — sound
    removal of provably-redundant realized safety checks). The optimizer is
    sound by construction: it only substitutes provably-equal values, deletes
    provably-dead effect-free instructions (demoting an impure dead-result store
    to an `Eval` so its effect and any trap are preserved), rewrites the CFG
    behavior-preservingly, or removes a check whose success edge is provably
    always taken. `MirProgram::verify` holds after every pass. Build modes are
    wired end to end (`run`/`mir --release-safe`/`--release-fast` optimize;
    Debug stays unoptimized unless `--opt`).
  - `k2c bench` — a reproducible benchmark harness that measures *executed VM
    instructions* (deterministic, not wall-clock) under Debug vs ReleaseFast
    over a committed set of bench programs, asserts the optimized output is
    byte-identical to the unoptimized output, and reports the reduction
    (~50% fewer instructions / ~2x across the suite). A differential corpus
    test asserts opt == unopt behavior in every mode (a single divergence is a
    blocker) and Debug == ReleaseSafe strictly.
  - **Concurrency (v0.11).** A deterministic **cooperative fiber scheduler** in
    `k2-vm` (`crate::sched`): each spawned unit of work is a green fiber with its
    own call-frame stack, and a single-threaded event loop interleaves ready
    fibers at explicit yield points (`spawn`/channel `send`-`recv`/`Mutex`
    acquire/`await`/`yield`). A FIFO ready queue plus FIFO waiter lists make the
    interleaving — and thus the output — reproducible run to run; an
    "all-fibers-blocked" state is reported as a clean deadlock diagnostic rather
    than a hang. The std concurrency surface is written in k2 over a small set of
    scheduler `@builtin` leaf intrinsics: `std.Thread.Executor`/`Task` (capability-
    passed spawn + join), `std.Channel(T)` (bounded/unbounded mpsc with blocking
    `send`/`recv` and `close`), `std.Thread.Mutex`/`WaitGroup`, `std.atomic.Value(T)`
    (`load`/`store`/`fetchAdd`/`swap`/`cmpxchg*` with explicit `Ordering`), and the
    colorless, keyword-free `std.event.Loop`/`Future` async surface
    (`loop.spawn(f, args)` + `future.await(loop, T)`). Every object is a value built
    from `sys.heap` and passed explicitly, never a global. OS-thread parallelism
    and the stackless async lowering are the native-backend realization of the
    same API; the VM realizes it via fibers (documented in
    `docs/spec/09-concurrency.md §8.1` and `crate::sched`). New example
    `examples/concurrency.k2` (spawn+join parallel sum, channel producer/consumer,
    mutex counter, atomics, async/await) runs with deterministic output.
  - **The build system is k2 + the package/module system (v0.12).** `build.k2`
    now *runs*: `build(b: *Build)` is ordinary k2 executed on the VM with a
    `*Build` **capability** — the build-time analogue of `*System`. Its methods
    bottom out in a floor of `@build*` **recording** intrinsics (no I/O, no real
    allocation — the comptime sandbox is honored) that build a deterministic,
    creation-ordered **build graph** the VM exposes after `build(b)` returns. The
    bundled `build` module (`crates/k2-std/std/build.k2`) declares the `Build`
    capability surface and its `Target`/`OptimizeMode`/`Step`/`Module`/`Artifact`
    helper types over that floor. A new `k2c build [step] [-Dkey=value …]`
    subcommand runs `build(b)`, parses `-Doptimize`/`-Dtarget`/custom options,
    writes a deterministic, reproducible `build.lock`, then executes the step:
    `install`/default **describes + validates** the DAG (native artifact emission
    is a documented no-op until post-0.13 native codegen), `run` **builds + runs**
    the chosen executable through the VM, and `test` **compiles + runs** the
    reachable `test { ... }` blocks. **Multi-file compilation** is realized by
    merging the module graph into one implicit-struct `SourceFile` (the
    std-injection move, generalized): `.k2` **path imports** (`@import("./x.k2")`)
    and **named modules** (`exe.addModule("name", lib.module())`, then
    `@import("name")` in the artifact) now resolve, type-check, monomorphize,
    lower, and run as one program — wired into `k2c run` as well, with the
    single-file fast path untouched. `@import("build_options")` is a **synthesized
    comptime module** (one `pub const` per `addOption`), so `if (opts.flag)` is a
    comptime-known condition whose dead branch the optimizer eliminates entirely.
    Fixes a latent checker/lowering bug where a **non-generic free function called
    through a namespace const** (`ns.add(x, y)`) was lowered with a spurious
    receiver. New `examples/support/root.k2` + `examples/tests/all.k2` make
    `examples/build.k2` run end to end: `k2c build` describes the DAG,
    `k2c build run -Dexample=hello` prints `Hello, k2!`, and `k2c build test` runs
    the example tests.

- **Native x86-64 backend foundation (v0.14).** A new pure-std crate
  `k2-codegen` that turns a *subset* of the monomorphized MIR into a real,
  static, directly-runnable x86-64 Linux ELF — with **no** Cranelift, **no**
  LLVM, **no** libc, and **no** third-party crates. It has three hand-rolled
  layers: a byte-exact **x86-64 instruction encoder** (REX/ModRM/SIB by hand:
  `mov`/`add`/`sub`/`imul`/`cqo`+`idiv`, `cmp`/`test`, `and`/`or`/`xor`,
  `shl`/`shr`/`sar`, `setcc`/`movzx`/`movsx`/`movsxd`, `lea`, `push`/`pop`,
  near `call`/`jmp`/`jcc` with `rel32` fixups, `syscall`, and the `[rbp - N]`
  stack-slot + immediate addressing modes), an **ELF64 writer** that emits a
  static non-PIE `ET_EXEC` at base `0x400000` (entry `0x401000`, one RX `PT_LOAD`
  for headers+code and an R-only `PT_LOAD` for `.rodata`, no dynamic linker / no
  section headers), and a **MIR → machine-code lowering** that gives each MIR
  local an `[rbp - 8*(i+1)]` stack slot and lowers width-correct integer
  arithmetic / compare / bitwise / shift, `Goto`/`Branch`/`Switch`/`Return`/
  `Trap`/`Unreachable`, System V AMD64 direct calls (args in
  `rdi/rsi/rdx/rcx/r8/r9`, result in `rax`, 16-byte-aligned call sites), the
  `@no_*_overflow`/`narrow_fits` safety predicates that guard a `Trap`, and the
  `write`/`exit` syscall intrinsics (`sys.io.stdout()`/`stderr()` → an fd token;
  a fixed-string `print` → a `write(fd, ptr, len)` of `.rodata` bytes; a `Trap`
  → `panic: …` on stderr + `exit(134)`, matching the VM). A `_start` shim runs
  `main` and `exit()`s with its result. Two new driver subcommands wire it in:
  **`k2c run-native <file.k2>`** compiles to a temp ELF, executes it, and
  propagates the exit code, and **`k2c build-native <file.k2> -o <out>`** writes
  the `chmod +x`-able ELF. Anything outside the subset (floats, aggregates,
  projected places, runtime-formatted `print`, …) is rejected up-front with a
  clean `error: native backend: …` message that points back to `k2c run` — it is
  never miscompiled, and all existing subcommands are untouched. The encoder
  asserts exact bytes against `as`/`objdump`-verified encodings and the ELF
  writer validates its header / segment invariants on **every** host; the
  native-execution tests (which actually **run** the emitted binary and assert
  exit code + stdout, **differentially against `k2c run`**) are gated to
  `x86_64`-Linux so CI exercises them while other hosts still build.

- **Native `*System` runtime — heap / clock / random / env via raw syscalls
  (v0.16).** `k2-codegen` now implements the `*System` capability floor in native
  machine code over **raw Linux x86-64 syscalls** (no libc, no crates), so
  heap-using programs run native == VM. A new `runtime` module emits hand-written
  support routines appended to `.text` (reached through a new `FixupKind::Runtime`
  relocation) plus a third, zero-mapped writable `PT_LOAD` (`p_filesz = 0`)
  holding the allocator registry, the deterministic clock counter, and the PRNG
  state (addressed via a new `FixupKind::State`):
  - an **`mmap`-backed heap** (`mmap`/`munmap`/`mprotect`, syscalls 9/11/10): one
    page-rounded region per allocation, prefixed by a **page-sized header**
    (`magic`/`total_len`/`payload_len`/`owner`/`live`/`next` in its first 40 bytes)
    so the payload starts on its own page boundary, handing back a real
    page-aligned payload address the existing pointer/slice codegen uses unchanged;
  - the **handle-based allocator registry** exactly mirroring the VM
    (`Default`/`GPA`/`Arena`/`FixedBuffer`): `@allocId`/`@allocHandle` mint and
    name handles, `create`/`alloc`/`free`/`realloc`/`destroy` dispatch on the
    handle, the `FixedBuffer` bumps a caller buffer (returning a real
    `error.OutOfMemory` on exhaustion), and the `Arena` bulk-frees on deinit. The
    registry has a fixed **`REG_MAX = 256`** slots (it lives in one page-rounded
    writable `PT_LOAD`); minting beyond it **traps cleanly** (`panic: too many
    allocators` + exit 134) rather than scribbling past the mapping. (The VM grows
    its allocator table unboundedly, so this hard cap is a documented native
    narrowing — never a wrong result, only a deterministic refusal.)
  - **GPA leak + double-free + use-after-free detection** matching the VM
    *observably*: `gpa.deinit()` returns whether anything leaked (so a leaking
    variant `@panic`s in Debug → clean exit 134), a double / invalid free traps
    (clean `panic: …` + exit 134); on free the whole payload is page-isolated and
    `mprotect`-ed `PROT_NONE`, so a use-after-free read or write — **at any offset,
    for any block size** — faults (**narrowing:** native UAF dies on `SIGSEGV` →
    exit 139 rather than the VM's clean 134; the acceptance corpus never commits a
    UAF on its success path, so its exit codes still match). A tracked-allocator
    `free`/`realloc` **unlinks** the freed block from the slot's live list and
    keeps it mapped (mirroring the VM's `retain`), so the single `deinit`
    reclamation walk is consistent and teardown never faults;
  - the **deterministic clock** (a monotonic counter advanced only by `sleep`, not
    `clock_gettime`) and the **reproducible splitmix64 PRNG** (re-implemented from
    the VM's seed, not `getrandom`), plus **offline-absent `env`** — all
    byte-identical native == VM;
  - the `_start` shim seeds the PRNG and the default-allocator slot before `main`;
    `ReleaseFast` strips the GPA tracking exactly like the VM's `checks_off`.
  Also new on the native path: `print` width/alignment padding (`{s:>14}`),
  `@errorName` (a `.rodata` name table), nested `[]const u8` array/struct literals,
  `MakeSlice` into a projected place, and a **field-slice word stride** that lets a
  *word-scalar generic container* — `std.ArrayList(u32)` / `List(u32)` — run
  natively: because the MIR shares one `deferred`-element method body across every
  `T`, the container's backing-store slice (reached through a struct field) is
  addressed in word-sized slots in both the generic methods and the concrete
  reader, so they agree (a standalone / array-view slice keeps its natural stride).
  **Acceptance:** `examples/errors.k2` (heap `create`/`destroy` + try/errdefer),
  `examples/allocators.k2` (leak-checking GPA + `ArrayList` + arena + a raw slice),
  and `examples/hello.k2` run **byte-identically native == VM** (verified by
  running the emitted binaries); the GPA leak detector works natively in both
  directions; and a differential corpus (alloc/free/create/destroy round-trips,
  `ArrayList(u32)` growth, leak / double-free traps, clock/random determinism, env)
  matches the VM. **Documented refusals (never miscompiled, fall to `k2c run`):** a
  generic container of an *aggregate* element — `List([]const u8)`, whose `> 8`-byte
  element cannot ride the shared scalar `deferred` value-parameter ABI losslessly
  (so `examples/generic_list.k2`, which instantiates `List([]const u8)`, stays
  VM-only this milestone) — plus the concurrency scheduler and the `*Build`
  capability, each surfaced as a clean `Unsupported` naming the construct.

- **Project infrastructure.** Continuous integration (`fmt` · `clippy` ·
  `build` · `test`, plus an examples smoke-test), contributor and security
  policies, dual MIT / Apache-2.0 licensing, and a development roadmap.

### Fixed

- **`k2c test` runner / coverage / fuzz (v0.24 review).** Nine verified defects in
  the first-class test runner, each with a regression test:
  - **Shared instruction budget never reset per test (BLOCKER).** The 200M-step
    budget was a single per-VM counter that only decremented, so a large suite
    exhausted it and spuriously FAILed a *later* correct test with "instruction
    budget exhausted" (a position-dependent, flaky verdict — also per fuzz
    iteration). `run_one_test` now resets `budget`/`started` per test, so each
    test/iteration gets the full budget; a genuinely infinite test still trips it
    *alone* while its neighbors pass.
  - **Coverage over-counted for path-import (merged) programs (MAJOR).** Std/prelude
    lines and functions were counted in the USER denominator and mislabeled with the
    user filename (e.g. a bogus `main2.k2:1195` in a 5-line file). The merge now
    records the std/build char-offset ranges and coverage excludes prelude code by
    *provenance*, so the merged report counts only user code.
  - **Line coverage attributed by bare global line number (MAJOR).** A line was
    credited "hit" whenever ANY function (including an excluded test body) landed on
    that line number. Coverage is now attributed per `(function, line)` code point,
    so a user line is covered only when a counted user function actually ran on it.
  - **`expectEqualSlices` rendered "expected N, found N" (MAJOR).** Equal-length but
    differing slices reported only the two (identical) lengths. A new `@testFailSlice`
    op scans for the first divergence: "slices differ at index I: expected X, found Y"
    for a content mismatch, "lengths differ: N vs M" for a length mismatch.
  - **`unreachable` trap caret on the wrong line (MINOR).** The trap inherited the
    *following* statement's line. The originating check's span is now recorded on the
    synthesized panic block, so the caret lands on the `unreachable` keyword.
  - **Merged-path FAIL/uncovered location mislabeled (NIT).** A failure/uncovered line
    in an imported file was labeled `<root>:<merged-line>`. A merged-source map now
    recovers and reports the true `(file, line)`.
  - **Fuzz determinism (MINOR) + `--fuzz-runs=0` silent PASS (NIT).** The shipped
    fuzz regression now also covers a guaranteed-trigger target (caught at iteration 0
    for every seed), the probabilistic nature of fuzzing is documented on
    `std.testing.fuzzInput`, and `--fuzz-runs=0` is rejected ("--fuzz-runs must be
    >= 1") instead of reporting an unexercised target as a pass.

- **Native miscompile: `for (slice) |x|` over a slice parameter summed to 0
  (v0.17 review #1).** A `&array` argument passed to a `[]const u32` parameter was
  typed by the checker as `*[N]T` and lowered as a single pointer (`OneInt`), but
  the callee's slice parameter is a fat `{ptr, len}` two-eightbyte value — so the
  native backend marshalled one register and the callee read a garbage `.len`,
  making `for (xs) |x| total += x;` loop zero times (`sum=0`) on native in every
  mode while the VM computed the real sum. Root-fixed in the MIR: the `&array`→slice
  coercion now emits a `MakeSlice` whenever the *destination* type is a slice
  (`lower_unary_into`'s `AddrOf` now prefers the destination local's slice type over
  the address expression's own `*[N]T`), and `callee_param_types` resolves the
  callee's parameter types from its AST signature when the callee is not yet
  lowered (forward/recursive calls), so the argument temp is correctly slice-typed.
  Both backends now see a real fat slice; `for`-over-slice value capture (and the
  `for (xs, 0..) |x, i|` value+index form, and `for`-over-array) yield `sum=100`
  native == VM in all modes. `bench/bench_slice_sum.k2` is re-included in the native
  bench differential gate (`native_bench_files`) so any future native≠VM slice
  divergence aborts the bench; its baseline instruction counts were regenerated.
- **Optimization-induced native divergence: const-folded integer printed with
  `{d}`/`{x}`/… refused in release modes (v0.17 review #2).** Constant folding
  collapsed a typed integer expression (e.g. a negative literal `const c: i64 = -7`)
  into an inline `Const::Int` whose *type* stayed `comptime_int` even though its
  value was masked to the sized destination, and the native print formatter only
  accepted `Type::Int`/`Bool`/`Deferred` — so the same program that ran in Debug
  native and on the VM failed to compile in `--release-safe`/`--release-fast` native
  (exit 1 "decimal format of a non-integer field"), an opt-vs-unopt native
  divergence. Fixed on both sides: the optimizer (`consts.rs`) now stamps a folded
  constant with the *sized* destination type when its result type is `comptime_int`
  (new `stamp_ty`, applied in `fold_unary`/`fold_binary`), and the native print
  renderers (`render_decimal/radix/char/default_field`) treat `Type::ComptimeInt`
  as a word-sized integer as defence-in-depth. A negative constant — and any
  const-foldable integer expression — printed with every integer verb now produces
  byte-identical output in all native modes == VM.
- **Unsound machine-level peephole: `mov r,0` → `xor r,r` across a live-flag
  block edge (v0.17 review #3).** The rule's flag-liveness check scanned only to
  the end of the current basic block and treated an unconditional `jmp`, a `label`,
  and the end of the function as proof the flags were dead — ignoring flags that are
  *live-out* across a jump to a successor block that opens with a `jcc`/`setcc`/`adc`.
  A `cmp; mov r,0; jmp L; … L: jcc` shape would rewrite the `mov` to an `xor` that
  clobbers the still-live flags, so the `jcc` branched on garbage (a latent
  miscompile, masked only by an unchecked front-end invariant). The rule is now
  sound by construction: it fires only when a flag-CLOBBERING instruction provably
  executes within the *same block* before any flag reader or block-exit edge
  (`flags_clobbered_before_use_in_block`); a `jmp`/`label`/end-of-function ends the
  window UNSAFE. This makes the rewrite fire rarely (the `mov r,0; …; call|ret|xor`
  shapes), which is the right trade — correctness over the tiny size win. The
  misleading "a call clobbers flags" comment is corrected: `CALL` preserves
  `EFLAGS`; the callee clobbers them by the SysV ABI.
- **ReleaseFast bounds-check stripping diverged native vs VM (v0.17 review #4).**
  An out-of-bounds index in `--release-fast` stripped the bounds check on native
  (reads OOB, exit 0) but the VM still kept its internal length test and *panicked*
  (exit 134), so `native == VM` did not hold per-mode for an OOB program. The VM
  now also strips the bounds check in ReleaseFast — an OOB index is clamped to the
  last element (a defined, non-trapping value), matching the documented "ReleaseFast
  reads clamped" semantics and the native backend's no-trap behavior. Both backends
  now exit 0 without panicking on an OOB access in ReleaseFast; Debug/ReleaseSafe
  still trap identically (134). Note: a genuine out-of-bounds *read* is undefined
  behavior — native reads adjacent stack (true garbage) while the VM yields the
  clamped element, so the *value* is backend-divergent and need not match; only the
  observable trap/exit behavior is now symmetric.
- **Native vs VM trap message text mismatch (v0.17 review #5).** The two
  trap-message tables disagreed on wording (native "negation overflow" /
  "cast truncates value" vs VM "negation of minimum integer" / "cast truncated
  value"), so a trap printed different stderr text on each backend even though exit
  codes matched. The native `trap_message` (`lower.rs`) is now byte-identical to the
  VM's `PanicInfo::message` (`vm.rs`) for every trap reason; a cross-referencing
  comment on both tables keeps them in lockstep.
- **Native heap: `realloc`/`free` + `deinit` teardown SIGSEGV (v0.16
  blocker).** A non-null `realloc` (or a `free`) through a TRACKED allocator
  (`GeneralPurposeAllocator` / `ArenaAllocator`) `munmap`-ed the old block
  immediately but left it threaded on the slot's intrusive `live_head` list; the
  single `deinit` reclamation walk then dereferenced that already-unmapped node
  and faulted (native exit 139) while the VM exited 0. The block is now **unlinked
  from `live_head` before reclamation and kept mapped** (matching the VM's
  `st.live.retain(...)` in `alloc_free`/`retrack_realloc`), so a freed/realloc-old
  block never feeds the teardown walk. The canonical pattern — `std.ArrayList`
  grown on a `GeneralPurposeAllocator` past its first `realloc`, then
  `list.deinit()` + `gpa.deinit()` — now exits 0 native == VM (the same fix covers
  the `ArenaAllocator` realloc + `arena.deinit()` path); leak and double-free
  detection across a `realloc` are unaffected.
- **Native `@allocId` registry overflow (v0.16 blocker).** `@allocId` minted
  handles via `reg_next++` with no bound check against the fixed `REG_MAX = 256`
  registry, so the 256th+ allocator scribbled past the writable state `PT_LOAD`
  and eventually segfaulted. `emit_alloc_id` now **bound-checks** the handle and
  traps cleanly (`panic: too many allocators` + exit 134) before writing out of
  bounds, converting silent corruption into a deterministic refusal.
- **Native use-after-free now traps for every freed payload.** Previously the
  freed payload was `mprotect`-ed `PROT_NONE` only over `[hdr+PAGE, hdr+total_len)`
  (and skipped entirely for sub-2-page blocks), leaving the first ~4 KB of payload
  readable — so a UAF read of `xs[0]` (or of any block ≤ 1 page) returned stale
  data with exit 0 instead of faulting like the VM. The header now occupies a full
  page so the payload is page-isolated, and `free` `mprotect`s the **entire**
  payload span: any UAF read/write, at any offset and any size, now faults (native
  139 vs the VM's clean 134), as the documented narrowing claims.
- **`k2-opt` inlining compile-time blow-up on cyclic call graphs.** Inlining
  accounting is now program-global: the recursion / global / per-caller inline
  budgets are threaded across every outer pass-manager iteration (previously the
  per-caller depth map was reborn each outer pass, so a recursive callee could be
  unrolled `RECURSION_BUDGET × OUTER_BUDGET` times and each copy reintroduced call
  sites the next pass unrolled again). The per-caller scan now resumes from the
  last inlined block and densifies once per caller instead of re-scanning the
  whole growing body and running `gc_unreachable_blocks` after every single
  inline, and the size gate measures the callee's *current* body (which may have
  grown on a cycle) rather than a stale summary. An 8-function mutual-recursion
  cycle that previously took ~10 s and produced ~5790 MIR blocks now compiles in
  under 0.1 s to ~129 blocks, byte-identical output. Inlining on the normal
  benchmarks is unaffected except a small, bounded reduction in recursive `fib`
  unrolling (still ~50% fewer executed instructions than Debug).
- **`MirProgram::verify` now checks all three `MakeSlice` operands.**
  `Rvalue::collect_locals` walked only `ptr`/`len`, so a dangling `offset` local
  in a `make_slice` slipped past the "no undefined local" invariant; it now walks
  `offset` too (the MIR pretty-printer also renders it).
- **Constant folding now masks comptime results like the VM.** A folded
  `Binary`/`Unary` whose result type is an unsized `comptime_int` stored into a
  sized local is now masked to the destination's width via the VM's `result_repr`
  fallback, matching the value the VM would compute at runtime exactly.

[Unreleased]: https://github.com/k2-lang/k2/commits/main
