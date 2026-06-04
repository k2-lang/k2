//! Multi-file compilation: the module graph merged into one `SourceFile`.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! A `.k2` file IS a struct (spec §08.1). The driver already proves this by
//! injecting `std` as a synthetic `const __k2_std_root = struct { <STD_BODY> };`
//! and re-pointing `const std = @import("std")` at it. **Multi-file compilation
//! is the same move, generalized:** every imported file is wrapped as a nested
//! `const __k2_mod_<hash> = struct { <file body> };`, the build module / build
//! options are injected the same way, and every `@import("...")` is rewritten to
//! a bare identifier reference at the namespace const. The whole merged text is
//! then parsed once and fed to the unchanged resolve → check → lower → opt → VM
//! pipeline, so path imports, named modules, `build`, and `build_options` all
//! resolve, type-check, monomorphize, lower, and run through the existing engine.
//!
//! The merge is purely textual (the proven std-injection mechanism), so it reuses
//! the parser and needs no AST surgery. Cycle handling is inherited from the
//! resolver's module walk: a file is wrapped at most once, so a mutually-recursive
//! import is simply a shared nested namespace (spec §08.2.3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use k2_mir::{lower_program, BuildMode, MirProgram};
use k2_resolve::{classify_import, resolve_file, ImportSpec};
use k2_types::check_file;
use k2_vm::BuildOptVal;

use crate::imports;

/// The inputs to a multi-file compile of one artifact.
pub struct CompileInputs {
    /// The artifact's root source file (absolute path).
    pub root_source: PathBuf,
    /// The build root — the directory `build.k2` lives in (for `build.lock`
    /// relative paths and cycle keys). For a standalone `k2c run multi.k2` this is
    /// the root file's own directory.
    pub build_root: PathBuf,
    /// Named modules wired in via `addModule`: `import_name -> file`.
    pub named_modules: Vec<(String, PathBuf)>,
    /// The artifact's build options (from `addOption`), surfaced through
    /// `@import("build_options")`.
    pub build_options: Vec<(String, BuildOptVal)>,
    /// Whether to inject the bundled `build` module (only a `build.k2` needs it).
    pub inject_build: bool,
}

/// A diagnostic from the multi-file front end, with a human label and message.
#[derive(Clone, Debug)]
pub struct MultiDiag {
    /// The file/label the diagnostic is about.
    pub label: String,
    /// The diagnostic message.
    pub message: String,
}

/// The set of resolved input files of a compile, in deterministic (sorted) order
/// — used by the lockfile to fingerprint the exact sources that fed the build.
#[derive(Clone, Debug, Default)]
pub struct InputFiles {
    /// Each resolved `.k2` input: `(relative_path, absolute_path)`.
    pub files: Vec<(String, PathBuf)>,
}

/// The outcome of merging the module graph: the combined source text plus the set
/// of resolved input files (for the lock).
pub struct MergedProgram {
    /// The combined, import-rewritten source text (one `SourceFile`).
    pub source: String,
    /// The resolved input files.
    pub inputs: InputFiles,
}

/// Reads a `.k2` file from disk, mapping an I/O error to a labeled diagnostic.
fn read_file(path: &Path) -> Result<String, MultiDiag> {
    std::fs::read_to_string(path).map_err(|e| MultiDiag {
        label: path.display().to_string(),
        message: format!("cannot read `{}`: {e}", path.display()),
    })
}

/// Lexically normalizes a path (collapsing `.`/`..`) without touching the
/// filesystem, so a graph key is stable regardless of how a path was spelled.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for comp in out {
        buf.push(comp.as_os_str());
    }
    buf
}

/// A small, dependency-free FNV-1a hash of a path string, rendered as hex. Used
/// to give each distinct file a stable, collision-resistant synthetic namespace
/// name `__k2_mod_<hash>` (deterministic across runs).
fn path_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The synthetic namespace const name for an imported file, keyed on the file's
/// full NORMALIZED ABSOLUTE path. Keying on the absolute path (not the
/// human-facing `rel_to_root` display, which falls back to the bare basename for
/// files outside the build root) guarantees two distinct files that happen to
/// share a basename — e.g. `../shared.k2` and `../other/shared.k2` — never
/// collide on the same `__k2_mod_<hash>` namespace.
fn mod_name(abs: &Path) -> String {
    let key = abs.to_string_lossy().replace('\\', "/");
    format!("__k2_mod_{}", path_hash(&key))
}

/// The relative-to-build-root display path of `abs` (for module names + the
/// lock). Falls back to the absolute path's file name if it is not under root.
fn rel_to_root(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            abs.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| abs.to_string_lossy().into_owned())
        })
}

/// Walks the module graph from `inputs.root_source` (and the named-module roots),
/// following `.k2` path imports, and builds one merged, import-rewritten source
/// text. Returns the merged program text + the resolved input set, or the first
/// fatal load error.
///
/// Algorithm (the std move, generalized):
/// 1. Discover every reachable `.k2` file (root + named modules + transitive path
///    imports), assigning each a stable `__k2_mod_<hash>` namespace and recording
///    a `import-string -> namespace` map per file.
/// 2. Emit each *imported* file wrapped as `const __k2_mod_<h> = struct { body };`
///    (the root file's items stay at the outermost level — it IS the program).
/// 3. Append the std root, the build root (if any), and the synthesized
///    build_options root.
/// 4. Rewrite every `@import("...")` to its bare namespace identifier.
pub fn merge(inputs: &CompileInputs) -> Result<MergedProgram, MultiDiag> {
    let root = normalize(&inputs.root_source);
    let build_root = normalize(&inputs.build_root);

    // The named-module map: import name -> normalized absolute file path.
    let named: BTreeMap<String, PathBuf> = inputs
        .named_modules
        .iter()
        .map(|(n, p)| (n.clone(), normalize(p)))
        .collect();

    // Discover every reachable file, in deterministic order. `visited` maps a
    // normalized path to its relative display name; `order` preserves a stable
    // (leaf-after-root BFS) emission order.
    let mut discovered: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    let mut queue: Vec<PathBuf> = Vec::new();

    // Seed with the named-module roots first (so they are always present even if
    // the root file does not path-import them), then the root file.
    for p in named.values() {
        queue.push(p.clone());
    }
    queue.push(root.clone());

    // Whether some reachable file imports the ROOT file (a self-import or an
    // import cycle that passes back through the root). When true, the root is also
    // exposed as a re-export namespace so `@import("<root>")` resolves (spec
    // §08.2.3 permits cycles).
    let mut root_is_imported = false;

    while let Some(path) = queue.pop() {
        if discovered.contains_key(&path) {
            continue;
        }
        let rel = rel_to_root(&path, &build_root);
        discovered.insert(path.clone(), rel.clone());
        order.push(path.clone());
        // Parse to find this file's path imports; a missing file surfaces here.
        let src = read_file(&path)?;
        let pres = k2_parse::parse(&src);
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        for raw in imports::path_imports(&pres.file) {
            if let ImportSpec::Path(rel_imp) = classify_import(&raw) {
                let target = normalize(&dir.join(&rel_imp));
                if target == root {
                    root_is_imported = true;
                }
                if !discovered.contains_key(&target) {
                    queue.push(target);
                }
            }
        }
    }

    // Build the merged source. The root file stays at the outermost level; every
    // OTHER reachable file is wrapped as a nested namespace const.
    let mut out = String::new();

    // 1. Wrapped imported modules (everything except the root file), sorted by
    //    relative path for determinism. The namespace name is keyed on the file's
    //    absolute path so distinct same-basename files never collide.
    let mut wrapped: Vec<(&PathBuf, &String)> =
        discovered.iter().filter(|(p, _)| **p != root).collect();
    wrapped.sort_by(|a, b| a.1.cmp(b.1));
    for (path, _rel) in &wrapped {
        let body = read_file(path)?;
        let rewritten = rewrite_imports_text(&body, path, &named);
        out.push_str(&format!("const {} = struct {{\n", mod_name(path)));
        out.push_str(&rewritten);
        out.push_str("\n};\n");
    }

    // 2. The std root + (optionally) the build root + the build_options root.
    out.push_str(&k2_std::std_root_item_source());
    if inputs.inject_build {
        out.push_str(&k2_std::build_root_item_source());
    }
    out.push_str(&synth_build_options(&inputs.build_options));

    // 3. The root file's items at the outermost level (it IS the program — its
    //    `main`/`build` must stay a top-level entry the lowerer enqueues).
    let root_body = read_file(&root)?;
    let root_rewritten = rewrite_imports_text(&root_body, &root, &named);
    out.push_str(&root_rewritten);
    if !out.ends_with('\n') {
        out.push('\n');
    }

    // 4. When the root is the target of some import (a self-import or a cycle
    //    through the root), expose it as a re-export namespace whose name matches
    //    `import_replacement`'s `mod_name(root)`. Each public top-level item is
    //    re-aliased to the corresponding outermost binding, which is visible to
    //    this nested struct via the ordinary outward scope walk. The lowerer still
    //    finds `main`/`build` at the outermost level, so the program entry is
    //    unaffected.
    if root_is_imported {
        out.push_str(&synth_root_reexport(&root_body, &mod_name(&root)));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    // The resolved input set, in sorted relative order (for the lock).
    let mut files: Vec<(String, PathBuf)> = discovered
        .iter()
        .map(|(p, rel)| (rel.clone(), p.clone()))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(MergedProgram {
        source: out,
        inputs: InputFiles { files },
    })
}

/// Synthesizes the `__k2_build_options_root` namespace: one `pub const` per
/// recorded `addOption`, with the value as a k2 literal. Because each is a
/// comptime-known literal, `if (opts.flag)` is a comptime-known condition and the
/// optimizer eliminates the dead branch (spec §08.6.3). An artifact with no
/// options still gets an (empty) namespace so the import resolves.
fn synth_build_options(opts: &[(String, BuildOptVal)]) -> String {
    let mut s = String::from("const __k2_build_options_root = struct {\n");
    for (name, val) in opts {
        s.push_str(&format!(
            "    pub const {}: {} = {};\n",
            name,
            val.k2_type(),
            val.k2_literal()
        ));
    }
    s.push_str("};\n");
    s
}

/// Synthesizes a re-export namespace for the ROOT file, so a `@import` that
/// resolves back to the root (a self-import or an import cycle through the root)
/// has a real namespace to bind to. The root body itself stays at the outermost
/// level (its `main`/`build` must remain a top-level entry); this wrapper merely
/// re-aliases each of the root's public top-level items to the corresponding
/// outermost binding. A `pub const X = @import(...)` in the root is skipped
/// (re-aliasing a module binding would just chase the same import).
///
/// Each member is aliased through a *uniquely-named* top-level shim
/// (`const __k2_root_alias_<ns>_<name> = <name>;`) rather than `pub const V = V;`
/// directly: inside the wrapper struct, a bare `V` on the right-hand side would
/// resolve to the struct's OWN sibling member `V` (sibling items are visible to
/// each other), inlining to itself and recursing forever. The shim name cannot
/// collide with any member, so it resolves unambiguously to the file-level
/// binding. The shim relies on the resolver permitting a container-member
/// declaration to reuse a file-level item's name (both live in the member
/// namespace, reached qualified), so the `pub const V` member is not a spurious
/// shadow of the outermost `pub const V`.
fn synth_root_reexport(root_body: &str, ns_name: &str) -> String {
    let pres = k2_parse::parse(root_body);
    // A short, stable, collision-free suffix derived from the namespace name.
    let tag = ns_name.trim_start_matches("__k2_mod_");
    let mut shims = String::new();
    let mut members = String::new();
    for item in &pres.file.items {
        if let Some((name, is_pub, is_import)) = reexportable_item(item) {
            if is_pub && !is_import {
                let shim = format!("__k2_root_alias_{tag}_{name}");
                shims.push_str(&format!("const {shim} = {name};\n"));
                members.push_str(&format!("    pub const {name} = {shim};\n"));
            }
        }
    }
    format!("{shims}const {ns_name} = struct {{\n{members}}};\n")
}

/// Classifies a top-level item for root re-export: returns `(name, is_pub,
/// is_import)` for a named `const`/`fn`, or `None` for an anonymous item
/// (`test`/`comptime`). A `const X = @import(...)` is flagged `is_import` so the
/// re-export skips it.
fn reexportable_item(item: &k2_syntax::Item) -> Option<(&str, bool, bool)> {
    use k2_syntax::{Expr, Item};
    match item {
        Item::Const {
            name,
            value,
            is_pub,
            ..
        } => {
            let is_import = matches!(
                value,
                Expr::Builtin { name, .. } if name == "@import"
            );
            Some((name.as_str(), *is_pub, is_import))
        }
        Item::Fn { name, is_pub, .. } => Some((name.as_str(), *is_pub, false)),
        // A top-level `var` is mutable global state; re-aliasing it as a `const`
        // would copy, not share, so it is intentionally not re-exported.
        Item::Var { .. } | Item::Test { .. } | Item::Comptime { .. } => None,
    }
}

/// Rewrites every `@import("...")` in a file's source TEXT to a bare identifier
/// reference, using the same in-place AST rewrite + reprint the driver uses for
/// std. Returns the file body with imports re-pointed at their namespace consts.
///
/// Because a textual reprint of the whole file is lossy (comments, exact
/// formatting), we instead operate by *byte substitution* on the original text:
/// each `@import("X")` literal is replaced with the bare namespace name in place,
/// preserving everything else.
fn rewrite_imports_text(body: &str, file_path: &Path, named: &BTreeMap<String, PathBuf>) -> String {
    let dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    imports::rewrite_import_strings(body, |raw| import_replacement(raw, &dir, named))
}

/// Computes the bare namespace identifier an `@import("raw")` rewrites to, or
/// `None` to leave the import opaque (an unmapped bare name). The synthetic
/// namespace name is keyed on the imported file's NORMALIZED ABSOLUTE path
/// (matching `merge`'s wrapping), so a path import that resolves to the root file
/// rewrites to the root re-export namespace `merge` emits for it.
fn import_replacement(
    raw: &str,
    importer_dir: &Path,
    named: &BTreeMap<String, PathBuf>,
) -> Option<String> {
    match classify_import(raw) {
        ImportSpec::Named(name) => match name.as_str() {
            "std" => Some(k2_std::STD_ROOT_NAME.to_string()),
            // `@import("build")` is left OPAQUE: the `*Build` capability surface is
            // reached via value intrinsics on `b: *Build` (like `*System`), never
            // through this const. Leaving it as a module import also avoids
            // colliding the `const build = @import("build")` binding with a
            // `build.k2`'s own `pub fn build` (the resolver special-cases module
            // import consts, but a rewritten plain const would redeclare `build`).
            "build" => None,
            "build_options" => Some("__k2_build_options_root".to_string()),
            other => named.get(other).map(|p| mod_name(&normalize(p))),
        },
        ImportSpec::Path(rel) => {
            let target = normalize(&importer_dir.join(&rel));
            Some(mod_name(&target))
        }
    }
}

/// The single-file fast path detector: `true` if `source` contains any *path*
/// import (`@import("./x.k2")` / `@import("a/b.k2")`), in which case the caller
/// must route through the multi-file merge. A program with only named imports
/// (`std`, wired modules) and no path imports compiles on the existing fast path.
pub fn has_path_imports(source: &str) -> bool {
    let pres = k2_parse::parse(source);
    imports::path_imports(&pres.file)
        .iter()
        .any(|raw| matches!(classify_import(raw), ImportSpec::Path(_)))
}

/// Compiles a merged source text through resolve → check → lower, returning the
/// `MirProgram` or a list of front-end diagnostics. The merged text is one
/// `SourceFile`, so this is the ordinary single-file pipeline.
pub fn compile_merged(
    merged: &str,
    label: &str,
    mode: BuildMode,
) -> Result<MirProgram, Vec<MultiDiag>> {
    let pres = k2_parse::parse(merged);
    if !pres.is_ok() {
        return Err(pres
            .diagnostics
            .iter()
            .filter(|d| d.severity == k2_parse::Severity::Error)
            .map(|d| MultiDiag {
                label: label.to_string(),
                message: format!("{}:{}: {}", d.span.line, d.span.col, d.message),
            })
            .collect());
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        return Err(resolved
            .diagnostics
            .iter()
            .filter(|d| d.severity == k2_resolve::Severity::Error)
            .map(|d| MultiDiag {
                label: label.to_string(),
                message: format!("{}:{}: {}", d.span.line, d.span.col, d.message),
            })
            .collect());
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        return Err(typed
            .diagnostics
            .iter()
            .filter(|d| d.severity == k2_types::Severity::Error)
            .map(|d| MultiDiag {
                label: label.to_string(),
                message: format!("{}:{}: {}", d.span.line, d.span.col, d.message),
            })
            .collect());
    }
    let prog = lower_program(&pres.file, &resolved, typed, mode).map_err(|diags| {
        diags
            .iter()
            .map(|d| MultiDiag {
                label: label.to_string(),
                message: format!("{}:{}: {}", d.span.line, d.span.col, d.message),
            })
            .collect::<Vec<_>>()
    })?;
    if !prog.is_ok() {
        return Err(prog
            .diagnostics
            .iter()
            .filter(|d| d.severity == k2_mir::Severity::Error)
            .map(|d| MultiDiag {
                label: label.to_string(),
                message: format!("{}:{}: {}", d.span.line, d.span.col, d.message),
            })
            .collect());
    }
    Ok(prog)
}
