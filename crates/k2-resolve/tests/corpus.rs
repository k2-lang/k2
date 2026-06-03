//! Whole-file acceptance corpus: every `examples/*.k2` must resolve with zero
//! error-severity diagnostics.
//!
//! This is the hard acceptance gate of the milestone. The predeclared set, the
//! deferral rules, the capture/container scoping, the field-vs-binding model,
//! and the import handling were all chosen specifically so this passes without
//! weakening any diagnostic. The examples path is resolved relative to
//! `CARGO_MANIFEST_DIR`, the same idiom the parser's corpus test uses.

use std::path::PathBuf;

use k2_resolve::{dump_scopes, resolve_file};

/// The absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn all_examples_resolve_clean() {
    let dir = examples_dir();
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("examples dir must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let pres = k2_parse::parse(&src);
        assert!(
            pres.is_ok(),
            "{} did not parse cleanly: {:#?}",
            path.display(),
            pres.diagnostics
        );

        let r = resolve_file(&pres.file);
        assert!(
            r.is_ok(),
            "{} did not resolve cleanly:\n{:#?}",
            path.display(),
            r.errors().collect::<Vec<_>>()
        );

        // The dump must not panic and must be rooted at the predeclared scope.
        let dump = dump_scopes(&r);
        assert!(
            dump.starts_with("(scope #0 predeclared"),
            "{}: unexpected dump head:\n{dump}",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected at least 6 example files, saw {checked}"
    );
}
