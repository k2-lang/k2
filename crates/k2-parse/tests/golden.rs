//! Golden / round-trip tests for the S-expression printer.
//!
//! Each example file is parsed, printed with [`k2_parse::to_sexpr`], and
//! compared to a checked-in `tests/golden/<name>.sexpr`. A handful of small,
//! focused goldens (one per tricky construct) localize failures. Set
//! `K2_BLESS=1` to regenerate every golden in place.

use std::path::{Path, PathBuf};

use k2_parse::{parse, to_sexpr};

/// The `tests/golden/` directory for this crate.
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

/// The workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

/// Compares `actual` against the golden file at `path`, or rewrites it when
/// `K2_BLESS=1` is set.
fn check_golden(path: &Path, actual: &str) {
    if std::env::var("K2_BLESS").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|_| {
        panic!(
            "missing golden {}; regenerate with K2_BLESS=1",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "golden mismatch for {} (regenerate with K2_BLESS=1)",
        path.display()
    );
}

/// Parses a source snippet, asserts a clean parse, and returns its S-expr.
fn sexpr_of(src: &str) -> String {
    let res = parse(src);
    assert!(res.is_ok(), "snippet did not parse: {:#?}", res.diagnostics);
    to_sexpr(&res.file)
}

#[test]
fn example_goldens_round_trip() {
    let dir = examples_dir();
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let src = std::fs::read_to_string(&path).unwrap();
        let res = parse(&src);
        assert!(
            res.is_ok(),
            "{} did not parse: {:#?}",
            path.display(),
            res.diagnostics
        );
        let actual = to_sexpr(&res.file);
        let golden = golden_dir().join(format!("{stem}.sexpr"));
        check_golden(&golden, &actual);
        names.push(stem);
    }
    assert!(
        names.len() >= 6,
        "expected >= 6 examples, saw {}",
        names.len()
    );
}

/// Small focused goldens, one per tricky construct, so a regression localizes.
#[test]
fn focused_goldens() {
    let cases: &[(&str, &str)] = &[
        ("errset_merge", "fn f() (A || B)!*u32 { return; }\n"),
        ("typed_init", "const x = Packet{ .magic = 0xBEEF };\n"),
        (
            "for_range_capture",
            "fn f() void { for (xs, 0..) |*slot, i| { use(slot, i); } }\n",
        ),
        (
            "while_continue",
            "fn f() void { while (i < 20) : (i += 1) { step(); } }\n",
        ),
        (
            "switch_ranges",
            "fn f() u8 { return switch (e) { 1...3 => a, error.X => 1, else => 0, }; }\n",
        ),
    ];
    for (name, src) in cases {
        let actual = sexpr_of(src);
        let golden = golden_dir().join(format!("{name}.sexpr"));
        check_golden(&golden, &actual);
    }
}
