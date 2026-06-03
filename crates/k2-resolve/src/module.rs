//! The module graph: `@import` classification, the file loader, and cycle
//! detection.
//!
//! A k2 program is a graph of modules connected by `@import`. There are two
//! flavors of import, distinguished purely by the literal string (§08 2.1/2.2):
//!
//! * **Named / well-known** imports (`std`, `builtin`, `build`,
//!   `build_options`, `root`, or any manifest dependency name): an *opaque*
//!   external namespace. We never load a file for it, and member access on it
//!   (`std.heap.X`) resolves only the base. Unknown bare names are not an error
//!   in v0.4 — the build script supplies them.
//! * **Path** imports (the string ends in `.k2` or contains `/`): another k2
//!   source file, resolved relative to the importing file's directory and
//!   forbidden from escaping the package root. In a multi-file resolve these
//!   build the edges of the graph, over which we detect cycles and missing
//!   files.

use crate::ids::ModuleId;
use k2_syntax::Span;
use std::path::{Component, Path, PathBuf};

/// How an `@import` binding resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleRef {
    /// A well-known / named opaque namespace (e.g. `std`, or a manifest
    /// dependency). Carries the bare name.
    WellKnown(String),
    /// A resolved `.k2` path import, canonicalized relative to the importer.
    Path(PathBuf),
    /// An import that failed to resolve: a missing file, a path that escaped the
    /// package root, or an edge broken to cut a cycle.
    Unresolved,
}

/// One node of the module graph.
#[derive(Clone, Debug)]
pub struct ModuleNode {
    /// This node's stable id.
    pub id: ModuleId,
    /// How this module resolves.
    pub reference: ModuleRef,
    /// The import-site span of the first `@import` that introduced it.
    pub origin: Span,
}

/// The classification of an import string, computed purely lexically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportSpec {
    /// A bare name — a named/well-known package import.
    Named(String),
    /// A relative path import (the verbatim string, still relative).
    Path(String),
}

/// Classifies an `@import` argument string (already stripped of its surrounding
/// quotes) per spec §08: a string that ends in `.k2` *or* contains a `/` is a
/// path import; anything else is a named/well-known import.
pub fn classify_import(raw: &str) -> ImportSpec {
    if raw.ends_with(".k2") || raw.contains('/') {
        ImportSpec::Path(raw.to_string())
    } else {
        ImportSpec::Named(raw.to_string())
    }
}

/// Why a path import could not be turned into a usable file path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The resolved path would escape the package root (§08 2.1).
    EscapesRoot,
    /// The file does not exist or could not be read.
    Missing,
}

/// Reads and parses `.k2` files for the multi-file driver. The single-file
/// entry point uses a [`NullLoader`] that never performs I/O; the CLI provides a
/// real filesystem loader.
///
/// The loader returns an already-*parsed* [`SourceFile`](k2_syntax::SourceFile)
/// so the resolver crate need not depend on the parser — the driver (`k2c`)
/// wires `k2_parse::parse` into its loader. A load error (missing file, parse
/// failure, escaped root) is reported as a [`LoadError`].
pub trait FileLoader {
    /// Reads and parses a `.k2` source by canonical path.
    fn load(&self, path: &Path) -> Result<k2_syntax::SourceFile, LoadError>;
}

/// Why a [`FileLoader`] could not produce a parsed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file does not exist or could not be read.
    Missing,
    /// The file existed but did not parse cleanly.
    ParseFailed,
}

/// A loader that performs no I/O. Used by the single-file `resolve_file` path,
/// where path imports are recorded as graph nodes but never followed.
pub struct NullLoader;

impl FileLoader for NullLoader {
    fn load(&self, _path: &Path) -> Result<k2_syntax::SourceFile, LoadError> {
        Err(LoadError::Missing)
    }
}

/// Resolves a relative path import against the importing file's directory,
/// rejecting any result that escapes `package_root`.
///
/// The join is *lexical* and normalized by hand (no filesystem access, so it
/// works for not-yet-existing paths and for the `NullLoader`): `.` components
/// are dropped and `..` pops the previous component, but a `..` that would pop
/// above `package_root` is an [`PathError::EscapesRoot`].
///
/// Note: the multi-file driver no longer uses the escape rejection to *gate*
/// imports — spec §08 2.1 lists `@import("../app.k2")` as valid, so a parent-
/// directory import that resolves to an existing file must load. The driver
/// uses [`resolve_path_lenient`] and lets file existence be the gate. This
/// function is retained for the lexical-normalization + containment *predicate*
/// it computes (and its unit tests).
pub fn resolve_path(
    importer_dir: &Path,
    rel: &str,
    package_root: &Path,
) -> Result<PathBuf, PathError> {
    let normalized = resolve_path_lenient(importer_dir, rel);
    let root = lexically_normalize(package_root);
    if normalized.starts_with(&root) {
        Ok(normalized)
    } else {
        Err(PathError::EscapesRoot)
    }
}

/// Resolves a relative path import against the importing file's directory,
/// returning the lexically-normalized target *without* any package-root
/// containment check. A `..` is allowed to climb above the importer's directory
/// (spec §08 2.1 permits `@import("../app.k2")`); whether the target actually
/// exists is decided later by the [`FileLoader`].
pub fn resolve_path_lenient(importer_dir: &Path, rel: &str) -> PathBuf {
    lexically_normalize(&importer_dir.join(rel))
}

/// Lexically normalizes a path: collapse `.`/`..` components without touching
/// the filesystem. A leading `..` that has nothing to pop is retained verbatim
/// (it can only ever fail the `starts_with(root)` test, which is what we want).
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a previous *normal* component; otherwise keep the `..`
                // (so escaping a relative root remains detectable).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_distinguishes_path_from_named() {
        assert_eq!(classify_import("std"), ImportSpec::Named("std".into()));
        assert_eq!(classify_import("build"), ImportSpec::Named("build".into()));
        assert_eq!(
            classify_import("math.k2"),
            ImportSpec::Path("math.k2".into())
        );
        assert_eq!(
            classify_import("geometry/shapes.k2"),
            ImportSpec::Path("geometry/shapes.k2".into())
        );
        // A bare slash (no `.k2`) is still a path import.
        assert_eq!(
            classify_import("sub/mod"),
            ImportSpec::Path("sub/mod".into())
        );
    }

    #[test]
    fn resolve_path_stays_within_root() {
        let root = Path::new("/pkg");
        let dir = Path::new("/pkg/src");
        assert_eq!(
            resolve_path(dir, "a.k2", root).unwrap(),
            PathBuf::from("/pkg/src/a.k2")
        );
        assert_eq!(
            resolve_path(dir, "../top.k2", root).unwrap(),
            PathBuf::from("/pkg/top.k2")
        );
    }

    #[test]
    fn resolve_path_rejects_escape() {
        let root = Path::new("/pkg");
        let dir = Path::new("/pkg/src");
        assert_eq!(
            resolve_path(dir, "../../etc/passwd.k2", root),
            Err(PathError::EscapesRoot)
        );
    }
}
