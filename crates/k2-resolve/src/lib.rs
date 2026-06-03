//! # k2-resolve — name resolution, scopes, and the module graph for k2
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the **HIR (high-level IR) layer** of the k2 front-end: a
//! resolved *side-table* over the existing [`k2_syntax`] AST. It does not build a
//! new tree. Instead it produces a [`Resolved`] value holding four parallel
//! structures keyed back to the AST by source [`Span`](k2_syntax::Span):
//!
//! * a **definition table** ([`Def`]) — every binding *site* (items, params,
//!   locals, fields, captures, imported modules, predeclared names);
//! * a **scope tree** ([`Scope`]) — the nested namespaces those bindings live in;
//! * a **uses table** ([`Use`]) — every identifier *occurrence* and the
//!   [`Resolution`] it was given; and
//! * a **module graph** ([`ModuleNode`]) — the `@import` namespaces this file
//!   pulls in, with cycle and missing-file detection in the multi-file driver.
//!
//! ## What is resolved, and what is deferred to type-checking (v0.5)
//!
//! Only identifier *references that denote a binding* are resolved: locals,
//! parameters, captures, file/container items, predeclared names, and imported
//! module names. Member access is **deferred**: for `obj.field` only `obj` is
//! resolved; the field name, `.EnumLiteral`s, `error.Literal`s, and initializer
//! field names are recorded as [`Resolution::DeferredMember`] (so `std.heap.X`,
//! `.windows`, `error.Empty`, and `Color{ .r = 1 }` all resolve clean) and left
//! for the type checker, which is the first phase that knows the base's type.
//!
//! ## Diagnostics
//!
//! Four error diagnostics are emitted, each with a precise span: use of an
//! undeclared identifier, a duplicate declaration in one scope, an illegal shadow
//! of an enclosing binding, and an import of a missing file. A structural import
//! *cycle* is surfaced as a **warning** (not an error): spec §08 2.3 permits
//! mutually-recursive file imports, so a cycle must not fail the build, but it is
//! still reported for tooling (and the back-edge is still cut to keep the walk
//! terminating). The [`Diagnostic`] shape mirrors `k2_parse::Diagnostic` so the
//! driver prints both with one formatter.
//!
//! ## A note on where the spec is silent
//!
//! Spec §01 5.3 makes shadowing a *predeclared* name explicitly legal, but the
//! spec does not state a rule for local-vs-local / local-vs-outer shadowing. Per
//! the milestone's direction, this crate implements a Zig-style **no-shadowing**
//! rule for user bindings (documented in [`resolver`]) and *allows* shadowing of
//! predeclared names. The predeclared set is a deliberate, documented superset of
//! §01 5.3 (it adds `f16`/`f128`, `anyopaque`, the capability types
//! `System`/`Allocator`/`Build`, and the `c_*` aliases) chosen so the canonical
//! examples resolve with zero diagnostics — see [`predeclared`].
//!
//! ## Single-file vs. multi-file
//!
//! [`resolve_file`] resolves one [`SourceFile`](k2_syntax::SourceFile) with no
//! I/O: path imports are recorded as graph nodes but not followed.
//! [`resolve_module`] walks a project across `.k2` path imports via a
//! [`FileLoader`], building the module graph and reporting cycles and missing
//! files.

mod def;
mod diag;
mod dump;
mod ids;
mod module;
mod predeclared;
mod resolver;
mod scope;
mod uses;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use def::{Def, DefKind, DefTable};
pub use diag::{Diagnostic, Severity};
pub use dump::{dump_resolution, dump_scopes};
pub use ids::{DefId, ModuleId, ScopeId};
pub use module::{
    classify_import, resolve_path, resolve_path_lenient, FileLoader, ImportSpec, LoadError,
    ModuleNode, ModuleRef, NullLoader, PathError,
};
pub use resolver::{Resolved, Resolver};
pub use scope::{Scope, ScopeKind, ScopeTree};
pub use uses::{Resolution, Use, Uses};

use k2_syntax::{Expr, Item, SourceFile};

/// Resolves a single source file into its [`Resolved`] side-table, performing no
/// I/O. Path imports are recorded as module-graph nodes but are not followed (so
/// no missing-file or cycle diagnostics arise); well-known imports stay opaque.
/// This is the fast path used by `k2c resolve <file>`.
pub fn resolve_file(file: &SourceFile) -> Resolved {
    Resolver::new().resolve(file)
}

/// One module's resolution within a multi-file build.
pub struct ModuleResolution {
    /// The canonical path of the file (the entry's path for the root).
    pub path: PathBuf,
    /// The file's resolved side-table.
    pub resolved: Resolved,
}

/// The result of resolving a whole module graph rooted at one entry file.
pub struct ResolvedModule {
    /// The index, in `modules`, of the entry file's resolution.
    pub root: usize,
    /// Every reachable module (including the root), keyed by canonical path.
    pub modules: Vec<ModuleResolution>,
    /// The graph-level diagnostics (missing files, import cycles) accumulated
    /// across the whole walk, on top of each file's own resolution diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

impl ResolvedModule {
    /// The entry file's resolution, if the entry loaded at all.
    pub fn root(&self) -> Option<&Resolved> {
        self.modules.get(self.root).map(|m| &m.resolved)
    }

    /// `true` if neither the graph nor any individual file produced an error.
    pub fn is_ok(&self) -> bool {
        self.diagnostics.iter().all(|d| !d.is_error())
            && self.modules.iter().all(|m| m.resolved.is_ok())
    }
}

/// Color of a node during the cycle-detecting DFS.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    /// On the current DFS path (a back-edge to a gray node is a cycle).
    Gray,
    /// Fully explored.
    Black,
}

/// Resolves a project starting at `entry`, following `.k2` path imports via
/// `loader`, building the module graph and detecting **missing files** and
/// (as a warning) **import cycles**.
///
/// Path imports resolve relative to the importing file's directory and may climb
/// into a parent directory (`@import("../app.k2")`, spec §08 2.1); file
/// existence — not an entry-directory containment rule — is what gates them, so
/// a parent-directory import of an existing file loads, while one that does not
/// exist is reported as missing. Mutually-recursive file imports (spec §08 2.3)
/// resolve with only a cycle *warning*, never a hard error.
///
/// Well-known imports (`std`, `build`, …) stay opaque and are never loaded.
pub fn resolve_module(entry: &Path, loader: &dyn FileLoader) -> ResolvedModule {
    let entry = normalize(entry);

    let mut walk = Walk {
        loader,
        colors: HashMap::new(),
        modules: Vec::new(),
        diagnostics: Vec::new(),
    };

    // Load + resolve the entry file. If it cannot be loaded the whole walk fails
    // gracefully with a synthetic empty root module.
    let root = match walk.loader.load(&entry) {
        Ok(file) => {
            walk.colors.insert(entry.clone(), Color::Gray);
            let resolved = resolve_file(&file);
            // The entry occupies index 0 of `modules`.
            let root_index = walk.modules.len();
            walk.modules.push(ModuleResolution {
                path: entry.clone(),
                resolved,
            });
            walk.visit_imports(&entry, &file);
            walk.colors.insert(entry.clone(), Color::Black);
            root_index
        }
        Err(_) => {
            walk.diagnostics.push(Diagnostic::error(
                k2_syntax::Span::default(),
                format!("cannot load entry module `{}`", entry.display()),
            ));
            let root_index = walk.modules.len();
            walk.modules.push(ModuleResolution {
                path: entry.clone(),
                resolved: empty_resolved(),
            });
            root_index
        }
    };

    ResolvedModule {
        root,
        modules: walk.modules,
        diagnostics: walk.diagnostics,
    }
}

/// The mutable state of a module-graph walk.
struct Walk<'a> {
    loader: &'a dyn FileLoader,
    colors: HashMap<PathBuf, Color>,
    modules: Vec<ModuleResolution>,
    diagnostics: Vec<Diagnostic>,
}

impl Walk<'_> {
    /// Visits every `@import` in `file` (already loaded from `importer`),
    /// following path imports and reporting cycles / missing files.
    fn visit_imports(&mut self, importer: &Path, file: &SourceFile) {
        let importer_dir = importer.parent().unwrap_or(Path::new(".")).to_path_buf();
        for item in &file.items {
            let (value, span) = match item {
                Item::Const { value, span, .. } => (value, *span),
                _ => continue,
            };
            let raw = match import_string(value) {
                Some(s) => s,
                None => continue,
            };
            // Named imports stay opaque; only path imports build edges.
            let rel = match module::classify_import(&raw) {
                ImportSpec::Named(_) => continue,
                ImportSpec::Path(p) => p,
            };
            // Resolve leniently: a parent-directory import (`../app.k2`) is valid
            // per spec §08 2.1, so we do *not* reject solely for leaving the
            // entry file's directory. File existence (the loader) is the gate;
            // a target that does not load is reported below as a missing import.
            let target = normalize(&module::resolve_path_lenient(&importer_dir, &rel));
            match self.colors.get(&target).copied() {
                Some(Color::Gray) => {
                    // A structural import cycle. Spec §08 2.3 explicitly permits
                    // mutually-recursive file imports ("Imports may refer to each
                    // other"), so this is a *warning*, not a hard error — only a
                    // genuine comptime-value dependency is illegal, and that is a
                    // type-/comptime-layer concern beyond v0.4 name resolution.
                    // The back-edge `continue` below still cuts the recursion.
                    self.diagnostics.push(Diagnostic::warning(
                        span,
                        format!(
                            "import cycle: `{}` -> `{}`",
                            importer.display(),
                            target.display()
                        ),
                    ));
                    continue; // break the cycle.
                }
                Some(Color::Black) => continue, // already fully explored.
                None => {}
            }
            match self.loader.load(&target) {
                Ok(child) => {
                    self.colors.insert(target.clone(), Color::Gray);
                    let resolved = resolve_file(&child);
                    self.visit_imports(&target, &child);
                    self.colors.insert(target.clone(), Color::Black);
                    self.modules.push(ModuleResolution {
                        path: target,
                        resolved,
                    });
                }
                Err(_) => {
                    self.diagnostics.push(Diagnostic::error(
                        span,
                        format!("import of missing file `{rel}`"),
                    ));
                }
            }
        }
    }
}

/// Extracts the literal argument of an `@import("...")`, if `value` is one.
fn import_string(value: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = value {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                let bytes = text.as_bytes();
                let inner =
                    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
                        &text[1..text.len() - 1]
                    } else {
                        text.as_str()
                    };
                return Some(inner.to_string());
            }
        }
    }
    None
}

/// Lexically normalizes a path (collapsing `.`/`..`) for use as a graph key.
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

/// An empty resolution, used when the entry module cannot be loaded at all.
fn empty_resolved() -> Resolved {
    resolve_file(&SourceFile::default())
}
