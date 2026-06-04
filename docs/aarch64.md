# aarch64 (ARMv8-A) native backend — cross-compilation

k2's native backend has a second target: **aarch64 Linux**, alongside the
original x86-64 Linux backend. The same monomorphized MIR that the x86-64 backend
compiles is lowered to a static, directly-runnable EM_AARCH64 ELF.

## Honest statement of what is and is not verified here

> **aarch64 support is cross-compiled and structurally validated (byte-exact
> encoder unit tests against the ARM ARM, plus `readelf -h`/`file` confirming a
> valid EM_AARCH64 static executable). It is NOT executed or disassembled in this
> environment — there is no emulator (`qemu-aarch64`), no aarch64 binutils, and the
> host `objdump` cannot disassemble aarch64. The binaries are expected to run on
> real aarch64 Linux but that has not been demonstrated here.**

Concretely:

- **Verified, on every host (including CI on x86-64 Linux):**
  - The aarch64 **instruction encoder** is byte-exact: ~45 unit tests assert each
    emitted 4-byte little-endian word against the value the ARM Architecture
    Reference Manual (DDI 0487) assigns to that instruction
    (`crates/k2-codegen/src/aarch64/tests.rs`). These build bytes only — nothing
    runs — so they pass everywhere. This is the primary correctness evidence for
    instruction selection (validated against the *published encoding*, not a
    running CPU).
  - The aarch64 **ELF** is structurally validated: tests parse the emitted header
    and assert `e_machine == EM_AARCH64 (183)`, `e_type == ET_EXEC`,
    `EI_CLASS == ELFCLASS64`, `EI_DATA == ELFDATA2LSB`, a valid entry point
    (`0x401000`), and a well-formed `PT_LOAD` with the kernel's
    `p_vaddr ≡ p_offset (mod p_align)` congruence
    (`crates/k2-codegen/src/tests.rs`, module `aarch64_cross`).
  - External tools confirm the file: where the host has them, `readelf -h` reports
    `Machine: AArch64` / `Type: EXEC` and `file` reports an *ARM aarch64*
    executable (`crates/k2c/tests/cli.rs`).
  - The **same-MIR property**: one MIR compiles to both a runnable EM_X86_64 ELF
    and a valid EM_AARCH64 ELF, proving the single lowering structure drives both
    targets.

- **NOT verified here, and stated plainly:**
  - aarch64 binaries are **never executed** — no `qemu-aarch64`, no aarch64
    hardware. We do **not** claim hello runs on aarch64.
  - aarch64 machine code is **never disassembled** — the host `objdump` knows only
    the i386/x86-64 BFD targets. Correctness rests on the byte-exact encoder tests
    vs the ARM ARM, not on a disassembler round-trip.

## Supported triples

`build-native`/`run-native` accept `--target=<triple>` (and the `-Dtarget=<triple>`
alias):

| Triple            | Status |
|-------------------|--------|
| `x86_64-linux`    | **Default.** Build + run + `native == VM` verified; ~24×–1537× faster than the VM. |
| `aarch64-linux`   | Cross-compile only. Structurally validated (encoder bytes + ELF headers + `readelf`/`file`); **not executed here**; expected to run on real aarch64 Linux. |

Aliases accepted: `x86_64-unknown-linux`, `x86_64-unknown-linux-gnu`,
`x86_64-linux-gnu`, `amd64-linux`; `aarch64-unknown-linux`,
`aarch64-unknown-linux-gnu`, `aarch64-linux-gnu`, `arm64-linux`. Any other triple
is rejected with a message listing the two supported triples.

`run-native --target=aarch64-linux` on an x86-64 host **refuses** to execute the
foreign-ISA binary (there is no emulator) and points you at `build-native`.

## Usage

```sh
# Cross-compile a hello-class program to a static aarch64 ELF.
k2c build-native --target=aarch64-linux examples/hello.k2 -o hello.aarch64

# Confirm it structurally (where binutils/file are present):
readelf -h hello.aarch64   # Machine: AArch64, Type: EXEC, Entry point 0x401000
file    hello.aarch64       # ELF 64-bit LSB executable, ARM aarch64, statically linked

# The explicit default is byte-identical to the implicit one:
k2c build-native --target=x86_64-linux examples/hello.k2 -o hello.x86_64
```

## Scope

The aarch64 lowering targets the **hello-class** subset: the AAPCS64 stack frame
and calling convention, parameter receipt in `x0`–`x7`, scalar
integer/compare/bitwise/shift arithmetic (width-correct), the `print` formatter
(literals + the `{s}`/`{d}`/`{}`/`{x}`/`{X}`/`{b}`/`{o}`/`{c}` verbs, including
64- and 128-bit decimals), the CFG terminators, the escaped-`main`-error path, the
safety-check `Trap` lowering, and the `write`/`exit_group` syscalls.

The `*System` **heap runtime** (the `mmap`-backed allocator and the deterministic
clock/PRNG) is **not yet ported** to aarch64. A program that reaches it is refused
with a clean `Unsupported` deferral rather than miscompiled:

```
the *System heap runtime is not yet ported to aarch64; cross-compile
hello-class programs or use the x86-64 backend / VM
```

Those programs (`errors`, `allocators`, …) remain fully supported and verified on
the x86-64 backend and the VM. A future milestone ports `runtime.rs` to a second
encoder-based emitter; the state-segment model and the `RuntimeFn` enum are
already target-neutral.

## Design notes

- **Fixed 32-bit instructions.** Every instruction is one little-endian `u32`.
  All arithmetic is 64-bit (`sf = 1`), mirroring the x86 backend's always-`REX.W`
  discipline; sub-64 width-correctness uses `ubfm`/`sbfm` (the bitfield-move
  primitives behind immediate shifts and zero/sign-extending narrows), exactly
  where the x86 path uses `movzx`/`movsx`/AND-masks.
- **Registers (roles).** The hello-class lowering keeps every local in a stack
  home (no register allocation) and threads operands through the AAPCS64
  caller-saved temporaries `x9`–`x15`, with `x16`/`x17` (IP0/IP1) as the address /
  print-cursor scratch. Arguments/return use `x0`–`x7`.
- **Remainder.** AArch64 has no remainder instruction, so `a % b` is
  `sdiv/udiv` into a temporary followed by `msub` (`a - (a/b)*b`).
- **Address materialization.** Because the ELF is non-PIE at a fixed base, a
  `.rodata` pointer is a known absolute 64-bit constant at link time. It is
  materialized with a 4-instruction `movz`+`movk×3` sequence and patched
  lane-by-lane (`patch_movz_movk`), keeping the relocation model identical to the
  x86 `mov r64, imm64` hole. (`adrp`/`add` is implemented and unit-tested for
  completeness but not used by the lowering.)
- **ELF / page size.** The writer targets a 4 KiB page (`p_align = 0x1000`) with
  both segments on a page boundary, satisfying the kernel's mapping congruence.
  aarch64 also supports 16K/64K pages; this writer documents and targets the 4 KiB
  configuration.
- **Syscalls.** `svc #0` with the number in `x8` and arguments in `x0`–`x5`; the
  numbers come from the aarch64 asm-generic ABI (`write = 64`, `read = 63`,
  `mmap = 222`, `munmap = 215`, `mprotect = 226`, `exit_group = 94`,
  `clock_gettime = 113`, `getrandom = 278`).
