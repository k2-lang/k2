//! Corpus acceptance: every `examples/*.k2` lowers to MIR with no errors in
//! Debug mode, the program verifies as well-formed, Debug MIR contains safety
//! checks, and ReleaseFast MIR has strictly fewer of them.

use std::path::PathBuf;

use k2_mir::{dump_mir, lower_program, BuildMode};
use k2_parse::parse;
use k2_resolve::resolve_file;
use k2_types::check_file;

/// The workspace's `examples` directory (two levels up from this crate).
fn examples_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p.push("examples");
    p
}

/// All `.k2` files in the examples directory, sorted.
fn example_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(examples_dir())
        .expect("examples dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "k2").unwrap_or(false))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected example files");
    files
}

/// Counts `*_check` lines in a dump (the safety scaffolding).
fn check_lines(dump: &str) -> usize {
    dump.lines().filter(|l| l.contains("_check")).count()
}

#[test]
fn every_example_lowers_cleanly_in_debug() {
    for path in example_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        let pres = parse(&src);
        assert!(pres.is_ok(), "{}: parse errors", path.display());
        let resolved = resolve_file(&pres.file);
        assert!(resolved.is_ok(), "{}: resolve errors", path.display());
        let typed = check_file(&pres.file, &resolved);
        assert!(typed.is_ok(), "{}: type errors", path.display());

        let prog = lower_program(&pres.file, &resolved, typed, BuildMode::Debug)
            .unwrap_or_else(|e| panic!("{}: lowering failed: {e:?}", path.display()));

        // No error-severity lowering/leak diagnostics.
        let errors: Vec<_> = prog.diagnostics.iter().filter(|d| d.is_error()).collect();
        assert!(
            errors.is_empty(),
            "{}: lowering diagnostics: {:?}",
            path.display(),
            errors
        );

        // Well-formed CFG.
        let problems = prog.verify();
        assert!(
            problems.is_empty(),
            "{}: not well-formed: {:?}",
            path.display(),
            problems
        );

        // Every block ends in a terminator that is not the placeholder default
        // unless explicitly Unreachable; that is guaranteed by construction, so
        // here we just assert each fn has at least one block.
        for f in &prog.funcs {
            assert!(
                !f.blocks.is_empty(),
                "{}: empty fn {}",
                path.display(),
                f.name
            );
        }
    }
}

#[test]
fn debug_has_checks_release_fast_has_fewer() {
    // Across the whole corpus, Debug MIR has safety checks and ReleaseFast has
    // strictly fewer (ideally zero).
    let mut total_debug = 0usize;
    let mut total_fast = 0usize;
    for path in example_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        let pres = parse(&src);
        let resolved = resolve_file(&pres.file);

        let typed_dbg = check_file(&pres.file, &resolved);
        let dbg = lower_program(&pres.file, &resolved, typed_dbg, BuildMode::Debug).unwrap();
        let dbg_dump = dump_mir(&dbg);

        let typed_fast = check_file(&pres.file, &resolved);
        let fast =
            lower_program(&pres.file, &resolved, typed_fast, BuildMode::ReleaseFast).unwrap();
        let fast_dump = dump_mir(&fast);

        let d = check_lines(&dbg_dump);
        let f = check_lines(&fast_dump);
        assert!(
            f <= d,
            "{}: ReleaseFast has more checks than Debug ({f} > {d})",
            path.display()
        );
        // ReleaseFast must have ZERO check lines.
        assert_eq!(
            f,
            0,
            "{}: ReleaseFast still has check lines",
            path.display()
        );
        total_debug += d;
        total_fast += f;
    }
    assert!(
        total_debug > 0,
        "Debug MIR must contain safety checks across the corpus"
    );
    assert_eq!(
        total_fast, 0,
        "ReleaseFast MIR must contain no safety checks"
    );
}
