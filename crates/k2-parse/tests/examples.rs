//! Whole-file acceptance corpus: every `examples/*.k2` must parse with zero
//! error-severity diagnostics.
//!
//! The path is resolved relative to `CARGO_MANIFEST_DIR` (this crate), so the
//! examples directory is `../../examples`.

use std::path::PathBuf;

use k2_parse::{parse, to_sexpr, Severity};

/// The absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn all_examples_parse_clean() {
    let dir = examples_dir();
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("examples dir must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let res = parse(&src);
        let errs: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            errs.is_empty(),
            "{} did not parse cleanly: {:#?}",
            path.display(),
            errs
        );
        // The S-expr printer must not panic and must produce a non-empty tree
        // rooted at `(source-file`.
        let sx = to_sexpr(&res.file);
        assert!(
            sx.starts_with("(source-file"),
            "{}: unexpected S-expr head",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected at least 6 example files, saw {checked}"
    );
}
