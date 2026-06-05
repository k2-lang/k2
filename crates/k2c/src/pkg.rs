//! The offline package manager's resolver: the local vendored registry, the
//! `k2.pkg` manifest reader, package content hashing, and the transitive
//! dependency resolution (semver selection + version-conflict + cycle detection).
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! ## Where this runs (the central constraint)
//!
//! Dependency *resolution* is **I/O** (it reads a registry directory, parses
//! manifests, and hashes bytes), so — honoring the comptime sandbox (spec §06.1 /
//! §08.6.1) — it must NOT happen inside `build(b)`. This module is the driver-side
//! **pre-resolve** pass: [`crate::build_cmd`] calls [`resolve_project`] BEFORE it
//! runs `build(b)`, producing a [`ResolvedDeps`] table that maps each declared
//! dependency name to a resolved root source path (plus version, source, content
//! hash, and the dependency's OWN resolved children). The VM later records only a
//! dependency *handle* and mints a synthetic library artifact rooted at the
//! resolved path; the proven multi-file merge machinery does the rest.
//!
//! ## The local registry (offline)
//!
//! There is no network and no real registry. "Fetching" = resolving against a
//! LOCAL vendored registry directory:
//!
//! ```text
//! <registry-root>/<pkg>/<version>/k2.pkg      # name, version, root_source, deps
//! <registry-root>/<pkg>/<version>/src/root.k2 # the package's nominated root
//! ```
//!
//! A `<version>` whose directory name is not a valid semver is ignored (so a
//! `README` dir is harmless). A path dependency bypasses the registry entirely:
//! its directory is read for its own `k2.pkg` (to pull in its transitive deps) but
//! it is pinned to the directory, not version-selected.
//!
//! ## The manifest (`k2.pkg`) — parsed, not executed
//!
//! `k2.pkg` is k2 syntax but we resolve it OFFLINE by reading a fixed, flat
//! `pub const package = .{ … };` literal STRUCTURALLY with a small dedicated
//! reader — executing it would require the comptime VM mid-resolution (circular
//! and I/O-laden). Identical bytes always read to identical structure, so the
//! lock is reproducible. A future networked manager can swap this reader without
//! touching the rest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::semver::{highest_match, Constraint, Version};

/// The declared source of one dependency: either a local path (bypasses version
/// resolution) or a registry semver constraint (resolved to the highest match).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// A local directory dependency. The string is the path AS WRITTEN in the
    /// manifest (relative to the manifest's own directory, or absolute).
    Path(String),
    /// A registry dependency: a semver constraint string, plus an optional
    /// registry-package-name override (default: the dependency name).
    Registry {
        /// The raw constraint text (`^1.0.0`, `~1.2`, …).
        version: String,
        /// The registry package name to resolve against (default: the dep name).
        registry_name: Option<String>,
    },
}

/// One dependency declaration parsed from a `k2.pkg`'s `.dependencies` table:
/// a name plus its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepDecl {
    /// The dependency name (the import name and default registry name).
    pub name: String,
    /// Where the dependency comes from.
    pub source: DepSource,
}

/// A parsed `k2.pkg` manifest: the package's own identity plus its declared
/// dependencies. Read structurally (never executed) by [`read_manifest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The package name (`.name`).
    pub name: String,
    /// The package version (`.version`), if declared (the root project may omit
    /// it). Registry packages MUST declare one.
    pub version: Option<String>,
    /// The package's root module path relative to its directory (`.root_source`,
    /// default `root.k2`).
    pub root_source: String,
    /// An optional registry-root override declared in the project manifest
    /// (`.registry = "vendor"`), relative to the manifest directory.
    pub registry: Option<String>,
    /// The declared dependencies, in sorted name order (deterministic).
    pub dependencies: Vec<DepDecl>,
}

/// One resolved dependency: the concrete version/source chosen for a declared
/// name, its on-disk directory + root file, its content hash, and the resolved
/// names of its OWN direct dependencies (so the synthetic library can wire them
/// as named modules, composing transitive deps through the same `addModule`
/// machinery).
#[derive(Clone, Debug)]
pub struct ResolvedDep {
    /// The dependency name (the import name, e.g. what `@import("calc")` uses).
    pub name: String,
    /// The resolved version, displayed in the lock (`1.2.0`). For a path dep this
    /// is the manifest's declared version (or `0.0.0` if absent).
    pub version: Version,
    /// The lockfile source descriptor: `registry`, `registry:<name>`, or
    /// `path:<as-written>`.
    pub source_desc: String,
    /// The package directory (absolute) holding `k2.pkg` + sources.
    pub dir: PathBuf,
    /// The package's root module path relative to `dir` (`src/root.k2`).
    pub root_rel: String,
    /// The absolute path to the root module (`dir/root_rel`).
    pub root_abs: PathBuf,
    /// A deterministic content hash of the whole package tree.
    pub hash: String,
    /// The names of this dependency's OWN direct dependencies (sorted), each of
    /// which is also present in the flat [`ResolvedDeps::deps`] table.
    pub children: Vec<String>,
}

/// The complete resolution result: a flat, deduplicated table (one entry per
/// package name) plus the resolved transitive edges (for the lock's `[graph]`).
#[derive(Clone, Debug, Default)]
pub struct ResolvedDeps {
    /// Every resolved package, keyed by name, in sorted order.
    pub deps: Vec<ResolvedDep>,
    /// The transitive `parent -> child` edges, sorted, for the lock's auditable
    /// resolution shape.
    pub edges: Vec<(String, String)>,
    /// The display string for the registry root used (for the lock header).
    pub registry_display: String,
}

impl ResolvedDeps {
    /// Looks up a resolved dependency by name.
    pub fn get(&self, name: &str) -> Option<&ResolvedDep> {
        self.deps.iter().find(|d| d.name == name)
    }
}

/// How the registry root was configured (for diagnostics).
#[derive(Clone, Debug)]
pub struct ResolveConfig {
    /// The configured registry root (absolute). May not exist if no registry dep
    /// is declared.
    pub registry_root: PathBuf,
    /// A short human display of the registry root (relative-ish, for the lock).
    pub registry_display: String,
    /// Locked versions from a present `deps.lock` (name → version string). When a
    /// locked version still satisfies the manifest constraint AND still exists in
    /// the registry, the resolver PINS to it instead of picking the newest match —
    /// so an ordinary build never silently moves a dependency (spec §7.3). Empty
    /// when no lock is present or `--update` is set.
    pub locked: BTreeMap<String, String>,
}

/// A clear, user-facing resolution diagnostic. The driver prints `message` to
/// stderr and exits nonzero — a missing/unsatisfiable/conflict/cycle is NEVER a
/// silent wrong build.
#[derive(Clone, Debug)]
pub struct ResolveError {
    /// The human-readable, single-line diagnostic.
    pub message: String,
}

impl ResolveError {
    fn new(message: impl Into<String>) -> ResolveError {
        ResolveError {
            message: message.into(),
        }
    }
}

// ===========================================================================
// Manifest reading (parse, NOT execute)
// ===========================================================================

/// Reads and structurally parses a `k2.pkg` manifest from `path`. Reads exactly
/// the fixed flat shape `pub const package = .{ .name=…, .version=…,
/// .root_source=…, .registry=…, .dependencies = .{ <name> = .{ .version|.path } }
/// };`. Any field it does not understand is ignored (forward-compat). A malformed
/// manifest (missing `.name`, or a dep with neither `.version` nor `.path`) is a
/// clear error.
pub fn read_manifest(path: &Path) -> Result<Manifest, ResolveError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ResolveError::new(format!("cannot read manifest `{}`: {e}", path.display()))
    })?;
    parse_manifest(&text, path)
}

/// Parses manifest TEXT (split out for unit testing). See [`read_manifest`].
fn parse_manifest(text: &str, path: &Path) -> Result<Manifest, ResolveError> {
    // Strip line comments so a `//` annotation never confuses the scanner; the
    // manifest grammar we read uses no `//` inside string literals in practice
    // (paths/versions contain none), so a simple per-line strip is safe and
    // deterministic.
    let cleaned = strip_line_comments(text);
    let label = path.display();

    let name = scan_string_field(&cleaned, ".name").ok_or_else(|| {
        ResolveError::new(format!("{label}: missing `.name` in `pub const package`"))
    })?;
    let version = scan_string_field(&cleaned, ".version");
    let root_source =
        scan_string_field(&cleaned, ".root_source").unwrap_or_else(|| "root.k2".to_string());
    let registry = scan_string_field(&cleaned, ".registry");

    let dependencies = scan_dependencies(&cleaned, &label)?;

    Ok(Manifest {
        name,
        version,
        root_source,
        registry,
        dependencies,
    })
}

/// Removes `//` line comments from each line, preserving everything before the
/// comment marker. (The manifest values never contain `//`.)
fn strip_line_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let code = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

/// Scans for a `.<field> = "value"` assignment and returns the unescaped string
/// value, or `None` if the field is absent. Matches the FIRST occurrence at the
/// top level of the manifest (the `.dependencies` block uses `.path`/`.version`
/// too, but those are scanned separately by [`scan_dependencies`], which operates
/// on the dependencies sub-text only).
fn scan_string_field(text: &str, field: &str) -> Option<String> {
    // Only scan the portion BEFORE `.dependencies` for the top-level identity
    // fields, so a dep's own `.version` is never mistaken for the package's.
    let head = match text.find(".dependencies") {
        Some(i) => &text[..i],
        None => text,
    };
    find_string_assignment(head, field)
}

/// Finds `.<field> = "..."` anywhere in `region` and returns the decoded string.
fn find_string_assignment(region: &str, field: &str) -> Option<String> {
    let needle = field;
    let mut search_from = 0usize;
    while let Some(rel) = region[search_from..].find(needle) {
        let at = search_from + rel;
        // Ensure this is a field token (preceded by non-identifier, the `.` is in
        // the needle) and followed by an `=` after optional whitespace.
        let after = &region[at + needle.len()..];
        let after_trim = after.trim_start();
        if let Some(rest) = after_trim.strip_prefix('=') {
            let rest = rest.trim_start();
            if let Some(val) = parse_string_literal(rest) {
                return Some(val);
            }
        }
        search_from = at + needle.len();
    }
    None
}

/// Parses a leading double-quoted string literal from `s`, decoding `\"`, `\\`,
/// `\n`, and `\t`. Returns `None` if `s` does not start with `"`.
fn parse_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 1;
            match bytes.get(i) {
                Some(b'n') => out.push('\n'),
                Some(b't') => out.push('\t'),
                Some(b'"') => out.push('"'),
                Some(b'\\') => out.push('\\'),
                Some(&c) => out.push(c as char),
                None => break,
            }
            i += 1;
        } else if b == b'"' {
            return Some(out);
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    None
}

/// Parses the `.dependencies = .{ <name> = .{ .version="…" | .path="…" }, … }`
/// block into a sorted list of [`DepDecl`]. Returns an empty list if the package
/// declares no dependencies. A dep entry with neither `.version` nor `.path` is a
/// clear error.
fn scan_dependencies(
    text: &str,
    label: &impl std::fmt::Display,
) -> Result<Vec<DepDecl>, ResolveError> {
    let Some(block_start) = text.find(".dependencies") else {
        return Ok(Vec::new());
    };
    // Find the opening brace of the `.{ … }` literal after `.dependencies =`.
    let after = &text[block_start..];
    let Some(brace_rel) = after.find('{') else {
        return Ok(Vec::new());
    };
    let body_start = block_start + brace_rel + 1;
    // Find the matching close brace by counting nesting.
    let body = match_braces(&text[body_start..]).ok_or_else(|| {
        ResolveError::new(format!(
            "{label}: malformed `.dependencies` block (unbalanced braces)"
        ))
    })?;
    let block = &text[body_start..body_start + body];

    let mut deps: Vec<DepDecl> = Vec::new();
    // Each entry is `<name> = .{ ... }`; iterate by locating `= .{` markers.
    let mut idx = 0usize;
    while idx < block.len() {
        // Find the next entry's `.{` (the inner struct literal).
        let Some(eq_rel) = block[idx..].find('{') else {
            break;
        };
        let inner_open = idx + eq_rel + 1;
        // The dependency NAME is the identifier immediately before the `= .{` (or
        // `=.{`). Walk back over `{`, `.`, `=`, and whitespace to the identifier.
        let name = extract_dep_name(&block[..inner_open]).ok_or_else(|| {
            ResolveError::new(format!(
                "{label}: malformed dependency entry (no name before `.{{`)"
            ))
        })?;
        let inner_len = match_braces(&block[inner_open..]).ok_or_else(|| {
            ResolveError::new(format!(
                "{label}: malformed dependency `{name}` (unbalanced braces)"
            ))
        })?;
        let inner = &block[inner_open..inner_open + inner_len];

        let path = find_string_assignment(inner, ".path");
        let version = find_string_assignment(inner, ".version");
        let registry_name = find_string_assignment(inner, ".registry");
        let source = match (path, version) {
            (Some(p), _) => DepSource::Path(p),
            (None, Some(v)) => DepSource::Registry {
                version: v,
                registry_name,
            },
            (None, None) => {
                return Err(ResolveError::new(format!(
                    "{label}: dependency `{name}` has neither `.version` nor `.path`"
                )));
            }
        };
        deps.push(DepDecl { name, source });
        idx = inner_open + inner_len + 1;
    }

    deps.sort_by(|a, b| a.name.cmp(&b.name));
    deps.dedup_by(|a, b| a.name == b.name);
    Ok(deps)
}

/// Extracts the dependency name: the last identifier token in `prefix` (which ends
/// just before the inner `.{`'s open brace), skipping the trailing `=`, `.`, and
/// whitespace. Handles both `name = .{` and the field-style `.name = .{`.
fn extract_dep_name(prefix: &str) -> Option<String> {
    // Trim trailing `{`, `.`, `=`, whitespace.
    let trimmed = prefix.trim_end_matches(['{', '.', '=', ' ', '\t', '\n', '\r']);
    // The identifier is the trailing run of identifier bytes.
    let end = trimmed.len();
    let mut start = end;
    let bytes = trimmed.as_bytes();
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    let name = &trimmed[start..end];
    // A leading `.` (field syntax) is already trimmed; the name is the bare ident.
    Some(name.to_string())
}

/// Given `s` starting just AFTER an opening brace, returns the byte length of the
/// brace's body (the count of bytes up to but not including the matching close
/// brace), accounting for nesting. `None` if unbalanced.
fn match_braces(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut in_str = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

// ===========================================================================
// Registry listing + content hashing
// ===========================================================================

/// Lists the available semver versions of package `pkg` under `registry_root`,
/// sorted ascending. A directory name that is not a valid semver is ignored (so a
/// README dir is harmless). Returns an empty vector if the package directory does
/// not exist.
fn list_registry_versions(registry_root: &Path, pkg: &str) -> Vec<Version> {
    let pkg_dir = registry_root.join(pkg);
    let mut versions: Vec<Version> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&pkg_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if let Some(v) = Version::parse(name) {
                    versions.push(v);
                }
            }
        }
    }
    versions.sort();
    versions
}

/// Computes a deterministic content hash of a package tree at `dir`: every `*.k2`
/// file plus the `k2.pkg`, hashed by relative path then bytes, in sorted order
/// (no mtimes). Any change to a dependency's bytes flips this hash and the lock.
pub fn hash_package(dir: &Path) -> String {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_tree(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (rel, abs) in &files {
        for &b in rel.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        if let Ok(bytes) = std::fs::read(abs) {
            for &b in &bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h ^= 0;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Recursively collects `*.k2` files and `k2.pkg` under `dir` into `out` as
/// `(rel_to_base, abs)` pairs. Deterministic: callers sort the result.
fn collect_tree(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect_tree(base, &p, out);
        } else {
            let is_k2 = p.extension().and_then(|e| e.to_str()) == Some("k2");
            let is_manifest = p.file_name().and_then(|n| n.to_str()) == Some("k2.pkg");
            if is_k2 || is_manifest {
                let rel = p
                    .strip_prefix(base)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.to_string_lossy().into_owned());
                out.push((rel, p));
            }
        }
    }
}

// ===========================================================================
// Transitive resolution (semver selection + conflict + cycle detection)
// ===========================================================================

/// One pending constraint on a package, tagged with the origin that imposed it
/// (the project, or a parent `pkg@version`) for a precise conflict diagnostic.
#[derive(Clone, Debug)]
struct Pending {
    /// The parsed constraint.
    constraint: Constraint,
    /// Where it came from (`project` or `foo@1.2.0`), for diagnostics.
    origin: String,
    /// The registry package name to resolve against (default: the dep name).
    registry_name: Option<String>,
}

/// Resolves the full transitive dependency graph for the root project manifest.
///
/// Seeds constraints from the project manifest, then repeatedly (in sorted name
/// order for determinism) selects the highest registry version satisfying the
/// INTERSECTION of all accumulated constraints on each package, pulls in that
/// version's own dependencies, and continues to a fixpoint. Path dependencies
/// bypass version selection (pinned to their directory) but contribute their own
/// transitive constraints. Detects version conflicts (an empty intersection) and
/// dependency cycles, reporting each clearly.
pub fn resolve_project(
    project_manifest: &Manifest,
    project_dir: &Path,
    config: &ResolveConfig,
) -> Result<ResolvedDeps, ResolveError> {
    // Accumulated registry constraints, by package name.
    let mut constraints: BTreeMap<String, Vec<Pending>> = BTreeMap::new();
    // Path dependencies, by name → (as-written path, absolute dir, manifest).
    let mut path_deps: BTreeMap<String, (String, PathBuf, Manifest)> = BTreeMap::new();
    // Finalized selections, by name.
    let mut selected: BTreeMap<String, ResolvedDep> = BTreeMap::new();
    // The transitive edges discovered.
    let mut edges: Vec<(String, String)> = Vec::new();
    // Adjacency for cycle detection: name → its direct dep names.
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // Seed from the project manifest. The project node is "project".
    seed_from_manifest(
        project_manifest,
        project_dir,
        "project",
        &mut constraints,
        &mut path_deps,
        &mut edges,
        &mut adjacency,
        &mut selected,
    )?;

    // Worklist to a fixpoint: process the lexicographically-first unresolved name.
    //
    // RE-SELECTION (finding: no re-selection under the full constraint set): a
    // registry package can be selected, then have its constraint set GROW when a
    // later transitive dependency is seeded. [`seed_from_manifest`] therefore
    // INVALIDATES any already-selected registry package whose constraint set just
    // grew (removing it from `selected`), so the worklist re-resolves it against
    // the FULL intersection — picking the highest version satisfying ALL
    // constraints, not just the subset known at first encounter. This loops to a
    // true fixpoint: constraints only ever accumulate (monotone), so the chosen
    // version is monotonically non-increasing and the process terminates.
    loop {
        // Resolve path deps first (they need no selection, just their manifest).
        let pending_path: Option<String> = path_deps
            .keys()
            .find(|n| !selected.contains_key(*n))
            .cloned();
        if let Some(name) = pending_path {
            let (as_written, dir, manifest) = path_deps.get(&name).unwrap().clone();
            let resolved = resolve_path_dep(&name, &as_written, &dir, &manifest)?;
            // Pull in the path dep's OWN transitive deps.
            seed_from_manifest(
                &manifest,
                &dir,
                &format!("{name} (path)"),
                &mut constraints,
                &mut path_deps,
                &mut edges,
                &mut adjacency,
                &mut selected,
            )?;
            selected.insert(name.clone(), resolved);
            continue;
        }

        // Then resolve the next registry package with constraints but no selection.
        let pending_reg: Option<String> = constraints
            .keys()
            .find(|n| !selected.contains_key(*n) && !path_deps.contains_key(*n))
            .cloned();
        let Some(name) = pending_reg else {
            break; // fixpoint reached
        };

        let pendings = constraints.get(&name).unwrap().clone();
        let resolved = resolve_registry_dep(&name, &pendings, config)?;
        // Read the chosen version's manifest to pull its transitive deps.
        let dep_manifest = read_manifest(&resolved.dir.join("k2.pkg"))?;
        seed_from_manifest(
            &dep_manifest,
            &resolved.dir,
            &format!("{}@{}", name, resolved.version),
            &mut constraints,
            &mut path_deps,
            &mut edges,
            &mut adjacency,
            &mut selected,
        )?;
        selected.insert(name.clone(), resolved);
    }

    // Detect cycles over the discovered adjacency (path + registry deps both
    // participate). A cycle is reported as `a -> b -> a`.
    if let Some(cycle) = find_cycle(&adjacency) {
        return Err(ResolveError::new(format!(
            "dependency cycle: {}",
            cycle.join(" -> ")
        )));
    }

    // Post-fixpoint conflict validation: re-check every selected package against
    // the INTERSECTION of ALL its accumulated registry constraints. With the
    // worklist's re-selection a registry package is already resolved against its
    // full set, so this is a belt-and-braces guard for it — but it is the ONLY
    // place a PATH dep's pinned version is checked against registry constraints
    // that a transitive dependency imposed on the same name (finding: a path dep
    // silently overriding a transitive registry version constraint). A path dep
    // may legitimately be the chosen SOURCE, but its declared version must still
    // satisfy every constraint the graph placed on that name, or it is a conflict
    // (a silent wrong build otherwise).
    for (name, pendings) in &constraints {
        // A package with only path-side parents (no registry `.version` term)
        // has an empty pending list; nothing to validate.
        let Some(first) = pendings.first() else {
            continue;
        };
        let Some(dep) = selected.get(name) else {
            continue;
        };
        let mut combined = first.constraint.clone();
        for p in &pendings[1..] {
            combined = combined.intersect(&p.constraint);
        }
        if !combined.matches(&dep.version) {
            let listed: Vec<String> = pendings
                .iter()
                .map(|p| format!("`{}` (from {})", p.constraint, p.origin))
                .collect();
            if path_deps.contains_key(name) {
                // The pinned source is a path dep whose version the registry
                // constraints reject: name the path pin and the unmet constraints.
                return Err(ResolveError::new(format!(
                    "version conflict on `{name}`: path dependency pins {} but {} — \
                     the path version must satisfy every registry constraint on `{name}`",
                    dep.version,
                    listed.join(" vs ")
                )));
            }
            return Err(ResolveError::new(format!(
                "version conflict on `{name}`: {} — no version satisfies all (selected {})",
                listed.join(" vs "),
                dep.version
            )));
        }
    }

    // Attach each resolved dep's direct children (its adjacency), sorted, so the
    // synthetic library can wire them as named modules.
    let mut deps: Vec<ResolvedDep> = Vec::new();
    for (name, mut dep) in selected {
        let mut children = adjacency.get(&name).cloned().unwrap_or_default();
        children.sort();
        children.dedup();
        dep.children = children;
        deps.push(dep);
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name));

    edges.sort();
    edges.dedup();

    Ok(ResolvedDeps {
        deps,
        edges,
        registry_display: config.registry_display.clone(),
    })
}

/// Seeds the constraint/path tables and adjacency from one manifest's declared
/// dependencies, recording an edge `parent -> dep` for each.
///
/// When a registry dependency adds a constraint to a package that was ALREADY
/// selected, that selection is now stale (it was chosen against a smaller
/// constraint set), so this removes it from `selected` to force the worklist to
/// re-resolve it under the grown set. See the worklist's re-selection note.
#[allow(clippy::too_many_arguments)]
fn seed_from_manifest(
    manifest: &Manifest,
    manifest_dir: &Path,
    parent: &str,
    constraints: &mut BTreeMap<String, Vec<Pending>>,
    path_deps: &mut BTreeMap<String, (String, PathBuf, Manifest)>,
    edges: &mut Vec<(String, String)>,
    adjacency: &mut BTreeMap<String, Vec<String>>,
    selected: &mut BTreeMap<String, ResolvedDep>,
) -> Result<(), ResolveError> {
    // The parent's adjacency-graph key: the bare package name (strip an origin
    // suffix like `@1.2.0` / ` (path)` so cycle detection keys on names).
    let parent_key = parent_name(parent);
    for dep in &manifest.dependencies {
        adjacency
            .entry(parent_key.clone())
            .or_default()
            .push(dep.name.clone());
        edges.push((parent_key.clone(), dep.name.clone()));
        adjacency.entry(dep.name.clone()).or_default();
        match &dep.source {
            DepSource::Path(p) => {
                // Resolve the path relative to the manifest's directory.
                let dir = normalize(&manifest_dir.join(p));
                let dep_manifest = read_manifest(&dir.join("k2.pkg")).map_err(|e| {
                    ResolveError::new(format!(
                        "dependency `{}` (path `{p}`): {}",
                        dep.name, e.message
                    ))
                })?;
                path_deps
                    .entry(dep.name.clone())
                    .or_insert((p.clone(), dir, dep_manifest));
            }
            DepSource::Registry {
                version,
                registry_name,
            } => {
                let constraint = Constraint::parse(version).ok_or_else(|| {
                    ResolveError::new(format!(
                        "dependency `{}`: invalid version constraint `{version}`",
                        dep.name
                    ))
                })?;
                let pending = Pending {
                    constraint,
                    origin: parent.to_string(),
                    registry_name: registry_name.clone(),
                };
                let bucket = constraints.entry(dep.name.clone()).or_default();
                // Skip an exact-duplicate constraint+origin (e.g. the same edge
                // re-seeded on a re-resolution loop) so we neither grow the set
                // spuriously nor re-invalidate a stable selection.
                let is_new = !bucket.iter().any(|p| {
                    p.constraint == pending.constraint
                        && p.origin == pending.origin
                        && p.registry_name == pending.registry_name
                });
                if is_new {
                    bucket.push(pending);
                    // A registry package selected earlier under a smaller set is now
                    // stale; drop it so the worklist re-resolves it against the full
                    // intersection. (Path deps are pinned, not re-selected here; the
                    // path-vs-registry conflict is caught in the post-fixpoint check.)
                    if !path_deps.contains_key(&dep.name) {
                        selected.remove(&dep.name);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The bare package name from an origin tag (`foo@1.2.0` → `foo`, `bar (path)` →
/// `bar`, `project` → `project`).
fn parent_name(origin: &str) -> String {
    let base = origin.split('@').next().unwrap_or(origin);
    base.split(" (").next().unwrap_or(base).trim().to_string()
}

/// Resolves a path dependency: pins it to its directory, reads its declared
/// version (or `0.0.0`), and hashes its tree. No version selection.
fn resolve_path_dep(
    name: &str,
    as_written: &str,
    dir: &Path,
    manifest: &Manifest,
) -> Result<ResolvedDep, ResolveError> {
    if !dir.exists() {
        return Err(ResolveError::new(format!(
            "dependency `{name}`: path `{as_written}` does not exist (looked in `{}`)",
            dir.display()
        )));
    }
    let version = manifest
        .version
        .as_deref()
        .and_then(Version::parse)
        .unwrap_or_else(|| Version::parse("0.0.0").unwrap());
    let root_rel = manifest.root_source.clone();
    let root_abs = normalize(&dir.join(&root_rel));
    if !root_abs.exists() {
        return Err(ResolveError::new(format!(
            "dependency `{name}`: root source `{root_rel}` not found in path dep `{as_written}`"
        )));
    }
    Ok(ResolvedDep {
        name: name.to_string(),
        version,
        source_desc: format!("path:{as_written}"),
        dir: dir.to_path_buf(),
        root_rel,
        root_abs,
        hash: hash_package(dir),
        children: Vec::new(),
    })
}

/// Resolves a registry dependency: intersects all accumulated constraints, picks
/// the highest satisfying version from the registry, and hashes its tree. Reports
/// a missing package, an unsatisfiable single constraint, and a multi-constraint
/// version conflict each clearly.
fn resolve_registry_dep(
    name: &str,
    pendings: &[Pending],
    config: &ResolveConfig,
) -> Result<ResolvedDep, ResolveError> {
    // The registry-package name may be overridden via `.registry = "<name>"` on
    // the dependency. Every parent that imports this name MUST agree on which
    // registry package it maps to: a `Some(override)` and the default name (the
    // bare dep name), or two distinct `Some` overrides, name DIFFERENT registry
    // packages with different code, so honoring only the first-seen and silently
    // dropping the rest is a wrong build (finding: registry_name override
    // collision). Collect every distinct effective registry name and require
    // unanimity; otherwise report a clear conflict naming the disagreeing origins.
    let reg_name: &str = {
        let mut distinct: Vec<(&str, &str)> = Vec::new(); // (reg_name, origin)
        for p in pendings {
            let eff = p.registry_name.as_deref().unwrap_or(name);
            if !distinct.iter().any(|(r, _)| *r == eff) {
                distinct.push((eff, p.origin.as_str()));
            }
        }
        if distinct.len() > 1 {
            let listed: Vec<String> = distinct
                .iter()
                .map(|(r, o)| format!("`{r}` (from {o})"))
                .collect();
            return Err(ResolveError::new(format!(
                "dependency `{name}` maps to conflicting registry packages: {}",
                listed.join(" vs ")
            )));
        }
        // Unanimous (possibly the default name): the single entry.
        distinct[0].0
    };
    let registry_root = &config.registry_root;

    let available = list_registry_versions(registry_root, reg_name);
    if available.is_empty() {
        // Distinguish "no registry at all" from "package not in registry".
        let have = list_registry_packages(registry_root);
        let have_disp = if have.is_empty() {
            "<empty>".to_string()
        } else {
            have.join(", ")
        };
        return Err(ResolveError::new(format!(
            "dependency `{name}`: package not found in registry `{}` (have: {have_disp})",
            config.registry_display
        )));
    }

    // Intersect all constraints into one.
    let mut combined = pendings[0].constraint.clone();
    for p in &pendings[1..] {
        combined = combined.intersect(&p.constraint);
    }
    if combined.is_empty() && pendings.len() > 1 {
        let listed: Vec<String> = pendings
            .iter()
            .map(|p| format!("`{}` (from {})", p.constraint, p.origin))
            .collect();
        return Err(ResolveError::new(format!(
            "version conflict on `{name}`: {} — no version satisfies all",
            listed.join(" vs ")
        )));
    }

    // Prefer a locked version when it still satisfies the (combined) constraint
    // and is still present — an ordinary build never silently moves a dependency.
    let locked_choice: Option<Version> = config
        .locked
        .get(name)
        .and_then(|s| Version::parse(s))
        .filter(|lv| combined.matches(lv) && available.iter().any(|v| v == lv));

    let chosen: Version = match locked_choice {
        Some(lv) => lv,
        None => highest_match(&available, &combined)
            .cloned()
            .ok_or_else(|| {
                let have: Vec<String> = available.iter().map(|v| v.to_string()).collect();
                if pendings.len() > 1 {
                    let listed: Vec<String> = pendings
                        .iter()
                        .map(|p| format!("`{}` (from {})", p.constraint, p.origin))
                        .collect();
                    ResolveError::new(format!(
                        "version conflict on `{name}`: {} — no version in registry satisfies all (have: {})",
                        listed.join(" vs "),
                        have.join(", ")
                    ))
                } else {
                    ResolveError::new(format!(
                        "dependency `{name}`: no version in registry satisfies `{}` (have: {})",
                        pendings[0].constraint,
                        have.join(", ")
                    ))
                }
            })?,
    };

    let dir = registry_root.join(reg_name).join(chosen.to_string());
    let manifest = read_manifest(&dir.join("k2.pkg"))
        .map_err(|e| ResolveError::new(format!("dependency `{name}` ({chosen}): {}", e.message)))?;
    let root_rel = manifest.root_source.clone();
    let root_abs = normalize(&dir.join(&root_rel));
    if !root_abs.exists() {
        return Err(ResolveError::new(format!(
            "dependency `{name}@{chosen}`: root source `{root_rel}` not found in registry package"
        )));
    }
    // The source descriptor records an override when the registry package name
    // differs from the dep name (none here, since reg_name == name in this path).
    let source_desc = if reg_name == name {
        "registry".to_string()
    } else {
        format!("registry:{reg_name}")
    };
    Ok(ResolvedDep {
        name: name.to_string(),
        version: chosen.clone(),
        source_desc,
        dir: dir.clone(),
        root_rel,
        root_abs,
        hash: hash_package(&dir),
        children: Vec::new(),
    })
}

/// Lists the package directory names directly under a registry root (for the
/// "have: …" hint in a missing-package diagnostic), sorted.
fn list_registry_packages(registry_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(registry_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(n) = entry.file_name().to_str() {
                    out.push(n.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// Detects a cycle in the dependency adjacency via DFS colouring, returning the
/// cycle path (`a -> b -> a`) if one exists. Deterministic: nodes are visited in
/// sorted order.
fn find_cycle(adjacency: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    // Colours: 0 = unvisited, 1 = on the active stack (grey), 2 = done (black).
    let mut colour: BTreeMap<String, u8> = BTreeMap::new();
    let mut stack: Vec<String> = Vec::new();
    // Visit in sorted order for a deterministic cycle report.
    let nodes: Vec<String> = adjacency.keys().cloned().collect();
    for node in &nodes {
        if colour.get(node).copied().unwrap_or(0) == 0 {
            if let Some(cycle) = dfs_cycle(node, adjacency, &mut colour, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

/// The recursive DFS for [`find_cycle`]: returns the cycle path when a back-edge
/// to a grey node is found.
fn dfs_cycle(
    node: &str,
    adjacency: &BTreeMap<String, Vec<String>>,
    colour: &mut BTreeMap<String, u8>,
    stack: &mut Vec<String>,
) -> Option<Vec<String>> {
    colour.insert(node.to_string(), 1);
    stack.push(node.to_string());
    if let Some(children) = adjacency.get(node) {
        let mut sorted = children.clone();
        sorted.sort();
        for child in &sorted {
            match colour.get(child).copied().unwrap_or(0) {
                0 => {
                    if let Some(cycle) = dfs_cycle(child, adjacency, colour, stack) {
                        return Some(cycle);
                    }
                }
                1 => {
                    // Back-edge: build the cycle path from `child` on the stack.
                    let start = stack.iter().position(|n| n == child).unwrap_or(0);
                    let mut cycle: Vec<String> = stack[start..].to_vec();
                    cycle.push(child.clone());
                    return Some(cycle);
                }
                _ => {}
            }
        }
    }
    stack.pop();
    colour.insert(node.to_string(), 2);
    None
}

/// Lexically normalizes a path (collapsing `.`/`..`) without touching the
/// filesystem — the same move `multi.rs` uses for stable graph keys.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("/tmp/k2.pkg")
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = parse_manifest(
            r#"pub const package = .{
                .name = "foo",
                .version = "1.2.0",
                .root_source = "src/root.k2",
            };"#,
            &p(),
        )
        .unwrap();
        assert_eq!(m.name, "foo");
        assert_eq!(m.version.as_deref(), Some("1.2.0"));
        assert_eq!(m.root_source, "src/root.k2");
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn defaults_root_source() {
        let m = parse_manifest(r#"pub const package = .{ .name = "bar" };"#, &p()).unwrap();
        assert_eq!(m.root_source, "root.k2");
        assert_eq!(m.version, None);
    }

    #[test]
    fn missing_name_is_error() {
        assert!(parse_manifest(r#"pub const package = .{ .version = "1.0.0" };"#, &p()).is_err());
    }

    #[test]
    fn parses_dependencies_registry_and_path() {
        let m = parse_manifest(
            r#"pub const package = .{
                .name = "app",
                .version = "0.1.0",
                .registry = "vendor",
                .dependencies = .{
                    .calc = .{ .version = "^1.0.0" },
                    .strutil = .{ .path = "../strutil" },
                },
            };"#,
            &p(),
        )
        .unwrap();
        assert_eq!(m.registry.as_deref(), Some("vendor"));
        assert_eq!(m.dependencies.len(), 2);
        // Sorted by name: calc, strutil.
        assert_eq!(m.dependencies[0].name, "calc");
        assert_eq!(
            m.dependencies[0].source,
            DepSource::Registry {
                version: "^1.0.0".to_string(),
                registry_name: None
            }
        );
        assert_eq!(m.dependencies[1].name, "strutil");
        assert_eq!(
            m.dependencies[1].source,
            DepSource::Path("../strutil".to_string())
        );
    }

    #[test]
    fn dep_without_source_is_error() {
        let r = parse_manifest(
            r#"pub const package = .{
                .name = "app",
                .dependencies = .{ .calc = .{ .other = "x" } },
            };"#,
            &p(),
        );
        assert!(r.is_err());
    }

    #[test]
    fn comments_are_stripped() {
        let m = parse_manifest(
            r#"// a comment
            pub const package = .{
                .name = "foo", // inline
                .version = "1.0.0",
            };"#,
            &p(),
        )
        .unwrap();
        assert_eq!(m.name, "foo");
    }

    #[test]
    fn cycle_detection() {
        let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
        adj.insert("a".to_string(), vec!["b".to_string()]);
        adj.insert("b".to_string(), vec!["a".to_string()]);
        let cycle = find_cycle(&adj).expect("must detect a->b->a");
        assert_eq!(cycle.first().map(|s| s.as_str()), Some("a"));
        assert_eq!(cycle.last().map(|s| s.as_str()), Some("a"));
    }

    #[test]
    fn no_cycle_in_dag() {
        let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
        adj.insert("a".to_string(), vec!["b".to_string(), "c".to_string()]);
        adj.insert("b".to_string(), vec!["c".to_string()]);
        adj.insert("c".to_string(), vec![]);
        assert!(find_cycle(&adj).is_none());
    }
}
