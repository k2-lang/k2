//! The compilation target abstraction (v0.18: a second native target).
//!
//! Through v0.17 the native backend hard-coded one target — x86-64 Linux — into
//! every layer: the [`crate::encode`] encoder, the [`crate::elf`] writer's
//! `e_machine`, the [`crate::lower`] register names and syscall numbers, and the
//! [`crate::link`] relocation widths. v0.18 introduces a SECOND target, aarch64
//! Linux, by factoring the *target-varying tables* into this module:
//!
//! * the ELF `e_machine` value ([`Target::e_machine`]),
//! * the Linux syscall-number map ([`Target::sysnr`]), and
//! * the supported-triple parsing the driver uses for `--target=<triple>`.
//!
//! The x86-64 path is preserved BIT-FOR-BIT: [`Target::X86_64Linux`] is the
//! default, and the existing x86 lowering/encoder/runtime are reached unchanged
//! through it. The aarch64 path ([`Target::Aarch64Linux`]) drives the new
//! [`crate::aarch64`] encoder + lowering and the EM_AARCH64 ELF header.
//!
//! ## Honesty note (verification constraint)
//!
//! aarch64 binaries are **cross-compiled and structurally validated** here, never
//! executed: this host has no `qemu-aarch64`, no aarch64 binutils, and its
//! `objdump` cannot disassemble aarch64. The aarch64 encoder is validated
//! byte-exact against the published ARM ARM (DDI 0487) encodings (see
//! [`crate::aarch64`]'s tests), and the emitted ELF is validated by parsing its
//! header (EM_AARCH64, a valid entry, a `PT_LOAD`) plus `readelf -h`/`file`. The
//! binaries are *expected* to run on real aarch64 Linux, but that has not been
//! demonstrated in this environment. See `docs/aarch64.md`.

/// A native compilation target: an ISA + OS pair the backend can emit a static
/// ELF for. The discriminant order is stable; [`Target::default`] is the host
/// x86-64 Linux target so every existing caller keeps its exact behavior.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Target {
    /// x86-64 Linux — the original, fully-executed native target (build + run +
    /// `native == VM` verified). The default, so every existing caller keeps its
    /// exact behavior.
    #[default]
    X86_64Linux,
    /// aarch64 (ARMv8-A) Linux — the v0.18 cross-compilation target. The emitted
    /// ELF is structurally validated but **not executed** in this environment
    /// (no emulator); see the module note.
    Aarch64Linux,
}

impl Target {
    /// The ELF `e_machine` value for this target: `EM_X86_64` (62 / `0x3E`) or
    /// `EM_AARCH64` (183 / `0xB7`).
    pub fn e_machine(self) -> u16 {
        match self {
            Target::X86_64Linux => 0x3E,
            Target::Aarch64Linux => 183,
        }
    }

    /// `true` if this target's emitted binary can be executed on the *current*
    /// host (i.e. it is the host ISA + OS). Used by `run-native` to refuse running
    /// a foreign-ISA binary instead of crashing the host with `Exec format error`.
    pub fn is_host(self) -> bool {
        match self {
            Target::X86_64Linux => cfg!(all(target_arch = "x86_64", target_os = "linux")),
            Target::Aarch64Linux => cfg!(all(target_arch = "aarch64", target_os = "linux")),
        }
    }

    /// The Linux syscall-number table for this target. Linux assigns *different*
    /// syscall numbers per architecture, so the lowering reads the numbers it
    /// needs from here rather than hard-coding them.
    pub fn sysnr(self) -> SysNr {
        match self {
            Target::X86_64Linux => SysNr {
                write: 1,
                read: 0,
                mmap: 9,
                munmap: 11,
                mprotect: 10,
                exit_group: 231,
                exit: 60,
                clock_gettime: 228,
                getrandom: 318,
            },
            // The aarch64 (generic / "asm-generic") Linux syscall ABI numbers.
            Target::Aarch64Linux => SysNr {
                write: 64,
                read: 63,
                mmap: 222,
                munmap: 215,
                mprotect: 226,
                exit_group: 94,
                // aarch64's asm-generic ABI has no legacy `exit`; the only
                // process-exit number is `exit_group` (94). The lowering uses
                // `exit_group` for both targets' process exit, so this alias keeps
                // the table total.
                exit: 94,
                clock_gettime: 113,
                getrandom: 278,
            },
        }
    }

    /// A short human-readable name (`x86_64-linux` / `aarch64-linux`).
    pub fn triple(self) -> &'static str {
        match self {
            Target::X86_64Linux => "x86_64-linux",
            Target::Aarch64Linux => "aarch64-linux",
        }
    }

    /// Parses a `--target=<triple>` / `-Dtarget=<triple>` value into a [`Target`].
    ///
    /// Accepts the canonical short forms plus the common GNU long triples as
    /// aliases. An unrecognized triple yields an error listing the supported set,
    /// so the driver can surface an actionable message rather than guessing.
    pub fn parse_triple(s: &str) -> Result<Target, String> {
        match s {
            "x86_64-linux"
            | "x86_64-unknown-linux"
            | "x86_64-unknown-linux-gnu"
            | "x86_64-linux-gnu"
            | "amd64-linux" => Ok(Target::X86_64Linux),
            "aarch64-linux"
            | "aarch64-unknown-linux"
            | "aarch64-unknown-linux-gnu"
            | "aarch64-linux-gnu"
            | "arm64-linux" => Ok(Target::Aarch64Linux),
            other => Err(format!(
                "unknown --target `{other}`; supported triples are \
                 `x86_64-linux` (default; build + run + native==VM) and \
                 `aarch64-linux` (cross-compile; structurally validated, not executed here)"
            )),
        }
    }
}

/// The Linux syscall numbers a target's lowering emits. Each field is the integer
/// the kernel dispatches on (placed in `rax` on x86-64 / `x8` on aarch64 before
/// the trapping instruction). The set is exactly the surface the native backend
/// reaches: I/O (`write`/`read`), the `mmap` heap runtime, process exit, the
/// deterministic clock, and randomness.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SysNr {
    /// `write(fd, buf, count)`.
    pub write: i64,
    /// `read(fd, buf, count)`.
    pub read: i64,
    /// `mmap(addr, len, prot, flags, fd, off)`.
    pub mmap: i64,
    /// `munmap(addr, len)`.
    pub munmap: i64,
    /// `mprotect(addr, len, prot)`.
    pub mprotect: i64,
    /// `exit_group(code)` — terminates the whole process.
    pub exit_group: i64,
    /// `exit(code)` — on aarch64's asm-generic ABI this aliases `exit_group`.
    pub exit: i64,
    /// `clock_gettime(clk, ts)`.
    pub clock_gettime: i64,
    /// `getrandom(buf, len, flags)`.
    pub getrandom: i64,
}
