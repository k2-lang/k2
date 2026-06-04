//! The build graph: the data structure `build(b)` records and `k2c build` reads.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! A `build.k2` is ordinary k2: its `pub fn build(b: *Build)` runs on the VM with
//! a `*Build` **capability** — the build-time analogue of `*System`. Where
//! `*System` bottoms out in the io/heap/clock intrinsics, `*Build` bottoms out in
//! a small floor of `@build*` *recording* intrinsics (see [`crate::isa`]). Those
//! intrinsics perform **no I/O and no real allocation** (honoring the comptime
//! sandbox of spec §06.1 / §08.6.1): they only push nodes into the [`BuildGraph`]
//! the VM exposes after `build(b)` returns. The driver then reads that graph and
//! executes the requested step.
//!
//! ## Determinism
//!
//! Every collection here is a `Vec` stored in **creation order** — never a
//! `HashMap` whose iteration order would vary. Options are insertion-ordered.
//! This makes the recorded graph, and therefore the lockfile the driver derives
//! from it, byte-for-byte reproducible for identical inputs.

/// A resolved target triple: `arch-os-abi`. The VM is the only backend at this
/// milestone, so the default is the host (`x86_64-linux-gnu`); a non-host triple
/// is recorded faithfully (native emission is a documented no-op until post-0.13
/// native codegen).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetTriple {
    /// The architecture (`x86_64`, `aarch64`, `riscv64`, `wasm32`).
    pub arch: String,
    /// The operating system (`linux`, `macos`, `windows`, `freebsd`, `wasi`,
    /// `other`).
    pub os: String,
    /// The ABI (`gnu`, `musl`, `msvc`, `none`).
    pub abi: String,
}

impl TargetTriple {
    /// The host triple the VM toolchain models by default.
    pub fn host() -> TargetTriple {
        TargetTriple {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            abi: "gnu".to_string(),
        }
    }

    /// Parses a `arch-os-abi` (or `arch-os`) triple string, filling a missing ABI
    /// with `gnu`. An empty or single-field string falls back to the host.
    pub fn parse(s: &str) -> TargetTriple {
        let parts: Vec<&str> = s.split('-').collect();
        match parts.as_slice() {
            [arch, os, abi] => TargetTriple {
                arch: (*arch).to_string(),
                os: (*os).to_string(),
                abi: (*abi).to_string(),
            },
            [arch, os] => TargetTriple {
                arch: (*arch).to_string(),
                os: (*os).to_string(),
                abi: "gnu".to_string(),
            },
            _ => TargetTriple::host(),
        }
    }

    /// The canonical `arch-os-abi` rendering.
    pub fn triple(&self) -> String {
        format!("{}-{}-{}", self.arch, self.os, self.abi)
    }

    /// The numeric index the `Target.Os` enum uses for `os`, so a `*Build`
    /// program can `switch (target.os)`. The order matches the `Os` enum declared
    /// in `build.k2`.
    pub fn os_index(&self) -> i128 {
        match self.os.as_str() {
            "linux" => 0,
            "macos" => 1,
            "windows" => 2,
            "freebsd" => 3,
            "wasi" => 4,
            _ => 5,
        }
    }

    /// The numeric index the `Target.Arch` enum uses for `arch`.
    pub fn arch_index(&self) -> i128 {
        match self.arch.as_str() {
            "x86_64" => 0,
            "aarch64" => 1,
            "riscv64" => 2,
            "wasm32" => 3,
            _ => 0,
        }
    }

    /// The numeric index the `Target.Abi` enum uses for `abi`.
    pub fn abi_index(&self) -> i128 {
        match self.abi.as_str() {
            "gnu" => 0,
            "musl" => 1,
            "msvc" => 2,
            "none" => 3,
            _ => 0,
        }
    }
}

/// The optimization mode the build was requested under. Matches the charter's
/// build-mode ladder; the numeric index matches the `OptimizeMode` enum in
/// `build.k2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptMode {
    /// `Debug`: Cranelift-style + full safety toolkit (the VM keeps checks on).
    Debug,
    /// `ReleaseSafe`: optimized, safety checks kept.
    ReleaseSafe,
    /// `ReleaseFast`: optimized, safety checks stripped.
    ReleaseFast,
}

impl OptMode {
    /// Parses a `-Doptimize=` value, defaulting to `Debug` on an unknown string.
    pub fn parse(s: &str) -> OptMode {
        match s {
            "ReleaseSafe" => OptMode::ReleaseSafe,
            "ReleaseFast" => OptMode::ReleaseFast,
            _ => OptMode::Debug,
        }
    }

    /// The enum-variant index the `OptimizeMode` enum uses (Debug=0, …).
    pub fn index(self) -> i128 {
        match self {
            OptMode::Debug => 0,
            OptMode::ReleaseSafe => 1,
            OptMode::ReleaseFast => 2,
        }
    }

    /// The canonical name.
    pub fn name(self) -> &'static str {
        match self {
            OptMode::Debug => "Debug",
            OptMode::ReleaseSafe => "ReleaseSafe",
            OptMode::ReleaseFast => "ReleaseFast",
        }
    }
}

/// A comptime-known build-option value, captured from a `addOption(T, name, v)`
/// call so it can be surfaced into the artifact's code via
/// `@import("build_options")`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildOptVal {
    /// A `bool` option.
    Bool(bool),
    /// A `[]const u8` string option.
    Str(String),
    /// An integer option (kept in full `i128`).
    Int(i128),
}

impl BuildOptVal {
    /// Renders this value as the k2 literal that the synthesized `build_options`
    /// module binds to a `pub const`.
    pub fn k2_literal(&self) -> String {
        match self {
            BuildOptVal::Bool(b) => b.to_string(),
            BuildOptVal::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            BuildOptVal::Int(i) => i.to_string(),
        }
    }

    /// The k2 type name for this value's `pub const T = ...;` declaration.
    pub fn k2_type(&self) -> &'static str {
        match self {
            BuildOptVal::Bool(_) => "bool",
            BuildOptVal::Str(_) => "[]const u8",
            BuildOptVal::Int(_) => "i64",
        }
    }

    /// A short, stable human/lock rendering.
    pub fn display(&self) -> String {
        match self {
            BuildOptVal::Bool(b) => b.to_string(),
            BuildOptVal::Str(s) => s.clone(),
            BuildOptVal::Int(i) => i.to_string(),
        }
    }
}

/// Which kind of artifact a graph node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A library (no `main`, exports a public surface).
    Library,
    /// An executable (`pub fn main(sys: *System) !void`).
    Executable,
    /// A test artifact (runs every reachable `test { ... }` block).
    Test,
    /// A run artifact: builds and runs the executable/test it wraps.
    Run,
}

impl ArtifactKind {
    /// The lock/describe keyword for this kind.
    pub fn keyword(self) -> &'static str {
        match self {
            ArtifactKind::Library => "lib",
            ArtifactKind::Executable => "exe",
            ArtifactKind::Test => "test",
            ArtifactKind::Run => "run",
        }
    }
}

/// One artifact node (a library, executable, test, or run wrapper).
#[derive(Clone, Debug)]
pub struct Artifact {
    /// This artifact's stable id (its index into [`BuildGraph::artifacts`]).
    pub id: u32,
    /// Which kind of artifact this is.
    pub kind: ArtifactKind,
    /// The artifact name (from the `.name` config field).
    pub name: String,
    /// The `root_source` path string (relative to the build root), if any. A
    /// `Run` artifact has none — it wraps `exe_id`.
    pub root_source: Option<String>,
    /// Named modules wired into this artifact: `(import_name, module_id)` in
    /// insertion order.
    pub modules: Vec<(String, u32)>,
    /// Build options surfaced to this artifact's code: `(name, value)` in
    /// insertion order.
    pub options: Vec<(String, BuildOptVal)>,
    /// For a `Run` artifact: the id of the executable/test it runs.
    pub exe_id: Option<u32>,
    /// For a `Run` artifact: whether `passForwardedArgs()` was called.
    pub forward_args: bool,
    /// The id of this artifact's embedded `step` (so `&run_exe.step` is a real
    /// step a user step can depend on).
    pub step_id: u32,
}

/// One module value (the result of `lib.module()`): the file it stands for.
#[derive(Clone, Debug)]
pub struct ModuleNode {
    /// This module's stable id.
    pub id: u32,
    /// The artifact whose `root_source` this module exposes.
    pub artifact_id: u32,
}

/// One named step in the build graph.
#[derive(Clone, Debug)]
pub struct StepNode {
    /// This step's stable id.
    pub id: u32,
    /// The step's command-line name (`run`, `test`, …), or `None` for an
    /// artifact-embedded step.
    pub name: Option<String>,
    /// The human description (for `--help`).
    pub desc: String,
    /// The step ids this step depends on (a DAG edge set), in insertion order.
    pub deps: Vec<u32>,
}

/// A build option declared via `b.option(T, name, desc)` (for `--help` + lock).
#[derive(Clone, Debug)]
pub struct DeclaredOption {
    /// The option name (`with-stderr-log`, `example`, …).
    pub name: String,
    /// The option's declared kind (`bool`, `string`, `enum`).
    pub kind: String,
    /// The human description.
    pub desc: String,
}

/// The whole recorded build graph: everything `build(b)` declared, in creation
/// order. This is what the driver reads back to describe / run / test.
#[derive(Clone, Debug)]
pub struct BuildGraph {
    /// User-selectable options declared with `b.option(...)`.
    pub options: Vec<DeclaredOption>,
    /// Every artifact (library/executable/test/run), in creation order.
    pub artifacts: Vec<Artifact>,
    /// Every `lib.module()` value, in creation order.
    pub module_nodes: Vec<ModuleNode>,
    /// Every step (named + artifact-embedded), in creation order.
    pub steps: Vec<StepNode>,
    /// The artifact ids passed to `b.installArtifact(...)`, in install order.
    pub install: Vec<u32>,
    /// The resolved target triple the build was requested under.
    pub target: TargetTriple,
    /// The optimization mode the build was requested under.
    pub optimize: OptMode,
}

impl BuildGraph {
    /// A fresh, empty graph seeded with the requested target + optimize mode.
    pub fn new(target: TargetTriple, optimize: OptMode) -> BuildGraph {
        BuildGraph {
            options: Vec::new(),
            artifacts: Vec::new(),
            module_nodes: Vec::new(),
            steps: Vec::new(),
            install: Vec::new(),
            target,
            optimize,
        }
    }

    /// Looks up an artifact by id.
    pub fn artifact(&self, id: u32) -> Option<&Artifact> {
        self.artifacts.get(id as usize)
    }

    /// Looks up a step by id.
    pub fn step(&self, id: u32) -> Option<&StepNode> {
        self.steps.get(id as usize)
    }

    /// Finds a named step by its command-line name.
    pub fn step_by_name(&self, name: &str) -> Option<&StepNode> {
        self.steps.iter().find(|s| s.name.as_deref() == Some(name))
    }

    /// The module node a `lib.module()` value with the given id refers to, mapped
    /// to the artifact whose `root_source` it exposes.
    pub fn module_artifact(&self, module_id: u32) -> Option<u32> {
        self.module_nodes
            .iter()
            .find(|m| m.id == module_id)
            .map(|m| m.artifact_id)
    }
}
