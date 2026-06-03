//! Negative name-resolution tests: each asserts that exactly the right single
//! diagnostic fires, with the right message and (where checked) span — proving
//! the resolver neither misses the error nor produces spurious extra ones.

use k2_resolve::{resolve_file, resolve_module, FileLoader, LoadError, Resolved};
use std::path::{Path, PathBuf};

/// Parses (asserting clean parse) and resolves `src`.
fn resolve(src: &str) -> Resolved {
    let pres = k2_parse::parse(src);
    assert!(
        pres.is_ok(),
        "snippet did not parse cleanly: {:#?}",
        pres.diagnostics
    );
    resolve_file(&pres.file)
}

/// The single error message, asserting there is exactly one error.
fn sole_error(r: &Resolved) -> String {
    let errs: Vec<_> = r.errors().collect();
    assert_eq!(
        errs.len(),
        1,
        "expected exactly one error, got {}: {:#?}",
        errs.len(),
        errs
    );
    errs[0].message.clone()
}

#[test]
fn undeclared_identifier() {
    let src = "fn f() i32 { return zzz; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert_eq!(msg, "use of undeclared identifier `zzz`");
    // The span points at `zzz` (offset of the first `z`).
    let err = r.errors().next().unwrap();
    let z = src.find("zzz").unwrap() as u32;
    assert_eq!((err.span.start, err.span.end), (z, z + 3));
}

#[test]
fn duplicate_file_scope_declaration() {
    let src = "const x = 1;\nconst x = 2;\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert_eq!(msg, "redeclaration of `x` in this scope");
    // The diagnostic points at the *second* declaration.
    let err = r.errors().next().unwrap();
    assert_eq!(err.span.line, 2);
}

#[test]
fn duplicate_block_local() {
    let src = "fn f() void { const y = 1; const y = 2; _ = y; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert_eq!(msg, "redeclaration of `y` in this scope");
}

#[test]
fn illegal_shadow_param_by_local() {
    let src = "fn f(x: i32) void { const x = 1; _ = x; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert!(
        msg.starts_with("declaration of `x` shadows an existing binding"),
        "got: {msg}"
    );
}

#[test]
fn illegal_shadow_outer_block_local() {
    // A nested-block local shadowing an outer-block local.
    let src = "fn f() void { const x = 1; { const x = 2; _ = x; } _ = x; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert!(
        msg.starts_with("declaration of `x` shadows an existing binding"),
        "got: {msg}"
    );
}

#[test]
fn illegal_shadow_of_file_item_by_local() {
    let src = "const g = 1;\nfn f() void { const g = 2; _ = g; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert!(
        msg.starts_with("declaration of `g` shadows an existing binding"),
        "got: {msg}"
    );
}

#[test]
fn block_locals_order_dependence_errors() {
    // `const a = b;` before `const b = 1;` — `b` is not yet in scope, proving
    // block locals are order-dependent.
    let src = "fn f() i32 { const a = b; const b = 1; return a + b; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert_eq!(msg, "use of undeclared identifier `b`");
}

#[test]
fn undeclared_label() {
    let src = "fn f() u32 { return blk: { break :nope 1; }; }\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert_eq!(msg, "use of undeclared label `nope`");
}

#[test]
fn non_canonical_or_overlong_width_integers_still_error() {
    // Finding 1, the rejection half. `u01` is non-canonical (leading zero) and
    // `u70000` exceeds the 65535 width cap; both fall through to the ordinary
    // undeclared-identifier path rather than resolving as primitives.
    for bad in ["u01", "u70000", "i099", "u123456789012345"] {
        let src = format!("fn f() void {{ var x: {bad} = 0; _ = x; }}\n");
        let r = resolve(&src);
        let msg = sole_error(&r);
        assert_eq!(
            msg,
            format!("use of undeclared identifier `{bad}`"),
            "`{bad}` must NOT resolve as a primitive integer"
        );
    }
}

#[test]
fn sibling_carveout_does_not_mask_local_over_local_shadow() {
    // Finding 3, the guard half. The sibling-item carve-out must NOT weaken a
    // genuine local-over-local shadow inside one function: `len` here is a
    // method *local*, not a sibling item, so reusing it in a nested block is
    // still an illegal shadow.
    let src = "\
const S = struct {\n\
    fn at() i32 { const len = 1; { const len = 2; _ = len; } return len; }\n\
};\n";
    let r = resolve(src);
    let msg = sole_error(&r);
    assert!(
        msg.starts_with("declaration of `len` shadows an existing binding"),
        "got: {msg}"
    );
}

// ---- Module-graph negatives (multi-file via a stub loader) -----------------

/// A loader serving a fixed in-memory map of canonical path -> source. A path
/// not in the map is [`LoadError::Missing`].
struct MapLoader {
    files: Vec<(PathBuf, String)>,
}

impl FileLoader for MapLoader {
    fn load(&self, path: &Path) -> Result<k2_syntax::SourceFile, LoadError> {
        let src = self
            .files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s.as_str())
            .ok_or(LoadError::Missing)?;
        let pres = k2_parse::parse(src);
        if pres.is_ok() {
            Ok(pres.file)
        } else {
            Err(LoadError::ParseFailed)
        }
    }
}

#[test]
fn missing_path_import() {
    // The entry imports `./nope.k2`, which the loader does not serve.
    let entry = PathBuf::from("/pkg/main.k2");
    let loader = MapLoader {
        files: vec![(
            entry.clone(),
            "const x = @import(\"./nope.k2\");\nfn f() void {}\n".to_string(),
        )],
    };
    let rm = resolve_module(&entry, &loader);
    let errs: Vec<_> = rm.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert_eq!(
        errs.len(),
        1,
        "expected one missing-import error: {errs:#?}"
    );
    assert!(
        errs[0].message.contains("import of missing file"),
        "got: {}",
        errs[0].message
    );
}

#[test]
fn import_cycle_is_detected_as_warning() {
    // a.k2 imports ./b.k2, b.k2 imports ./a.k2 — a path-import cycle. Spec §08
    // 2.3 permits mutually-recursive file imports, so this is surfaced as a
    // *warning*, never a hard error, and the walk does not hang/panic.
    let a = PathBuf::from("/pkg/a.k2");
    let b = PathBuf::from("/pkg/b.k2");
    let loader = MapLoader {
        files: vec![
            (a.clone(), "const b = @import(\"./b.k2\");\n".to_string()),
            (b.clone(), "const a = @import(\"./a.k2\");\n".to_string()),
        ],
    };
    let rm = resolve_module(&a, &loader);
    // No error-severity diagnostic, and the whole resolution is "ok".
    let errs: Vec<_> = rm.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert!(errs.is_empty(), "cycle must not error: {errs:#?}");
    assert!(rm.is_ok(), "legal mutual imports must resolve ok");
    // Exactly one cycle warning is recorded for tooling.
    let cycle_warns: Vec<_> = rm
        .diagnostics
        .iter()
        .filter(|d| !d.is_error() && d.message.contains("import cycle"))
        .collect();
    assert_eq!(
        cycle_warns.len(),
        1,
        "expected exactly one cycle warning: {:#?}",
        rm.diagnostics
    );
}

#[test]
fn parent_directory_path_import_resolves() {
    // Finding 5: a child importing `../sibling.k2` (an existing file) must
    // resolve with NO "escapes package root" error. Spec §08 2.1 lists
    // `@import("../app.k2")` as valid.
    let child = PathBuf::from("/pkg/src/feature.k2");
    let parent = PathBuf::from("/pkg/app.k2");
    let loader = MapLoader {
        files: vec![
            (
                child.clone(),
                "const root = @import(\"../app.k2\");\nfn f() void {}\n".to_string(),
            ),
            (parent.clone(), "pub fn app() void {}\n".to_string()),
        ],
    };
    let rm = resolve_module(&child, &loader);
    let errs: Vec<_> = rm.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert!(
        errs.is_empty(),
        "parent-directory import of an existing file must resolve clean: {errs:#?}"
    );
    assert!(rm.is_ok());
    // Both the entry and the parent module were loaded.
    assert_eq!(rm.modules.len(), 2, "entry + parent should both load");
}

#[test]
fn out_of_root_path_import_that_is_missing_is_rejected() {
    // A `..`-climbing import whose target the loader does not serve is reported
    // as a missing file (existence — not an entry-dir containment rule — is the
    // gate; a parent-directory import that *did* exist would load fine).
    let entry = PathBuf::from("/pkg/main.k2");
    let loader = MapLoader {
        files: vec![(
            entry.clone(),
            "const x = @import(\"../../etc/evil.k2\");\n".to_string(),
        )],
    };
    let rm = resolve_module(&entry, &loader);
    let errs: Vec<_> = rm.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert_eq!(
        errs.len(),
        1,
        "expected one missing-import error: {errs:#?}"
    );
    assert!(
        errs[0].message.contains("import of missing file"),
        "got: {}",
        errs[0].message
    );
}
