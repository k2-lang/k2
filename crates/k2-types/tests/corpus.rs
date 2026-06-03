//! Whole-file acceptance corpus: every `examples/*.k2` must type-check with zero
//! error-severity diagnostics.
//!
//! This is the hard acceptance gate of the milestone. The comptime-deferral
//! boundary (generic instantiation, reflection builtins, module/`anytype` member
//! access) is precisely calibrated so the examples check clean WITHOUT weakening
//! the concrete-core checks the negative suite verifies. If an example cannot
//! check cleanly, that signals a missing real type feature — not a reason to
//! widen `Deferred`.

use std::path::PathBuf;

use k2_types::{check_file, dump_signatures};

/// The absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn all_examples_check_clean() {
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
        let resolved = k2_resolve::resolve_file(&pres.file);
        assert!(
            resolved.is_ok(),
            "{} did not resolve cleanly: {:#?}",
            path.display(),
            resolved.errors().collect::<Vec<_>>()
        );

        let typed = check_file(&pres.file, &resolved);
        assert!(
            typed.is_ok(),
            "{} did not type-check cleanly:\n{:#?}",
            path.display(),
            typed.errors().collect::<Vec<_>>()
        );

        // The signature dump must be non-empty and deterministic.
        let sigs = dump_signatures(&typed, &resolved);
        assert!(
            !sigs.is_empty(),
            "{}: expected a non-empty signature dump",
            path.display()
        );
        let sigs2 = dump_signatures(&typed, &resolved);
        assert_eq!(
            sigs,
            sigs2,
            "{}: dump must be deterministic",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected at least 6 example files, saw {checked}"
    );
}
