//! Golden tests: each example formats to a checked-in canonical output, and
//! every golden is itself a fixed point of the formatter.
//!
//! Regenerate the goldens with `K2_FMT_BLESS=1 cargo test -p k2-fmt --test
//! golden`. Because the goldens are already canonical, feeding a golden back
//! through the formatter is a free extra idempotence check.

use std::fs;
use std::path::PathBuf;

use k2_fmt::format_source;

/// The example sources, embedded so the test stays offline.
const EXAMPLES: &[(&str, &str)] = &[
    ("hello.k2", include_str!("../../../examples/hello.k2")),
    (
        "allocators.k2",
        include_str!("../../../examples/allocators.k2"),
    ),
    ("errors.k2", include_str!("../../../examples/errors.k2")),
    (
        "generic_list.k2",
        include_str!("../../../examples/generic_list.k2"),
    ),
    (
        "comptime_reflection.k2",
        include_str!("../../../examples/comptime_reflection.k2"),
    ),
    ("build.k2", include_str!("../../../examples/build.k2")),
];

/// Absolute path to a golden file under `tests/golden/`.
fn golden_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("golden");
    p.push(format!("{name}.fmt"));
    p
}

/// Asserts `src` formats exactly to `want`.
fn expect(src: &str, want: &str) {
    let got = format_source(src).unwrap_or_else(|d| panic!("parse error: {d:?}"));
    assert_eq!(got, want);
}

#[test]
fn wraps_a_long_fn_signature() {
    // A signature wider than 100 columns breaks one parameter per line, with a
    // trailing comma, the return type and ` {` de-indented to the opener.
    let src = "fn parseDoubled(allocator: Allocator, text: []const u8, extra: usize) (ParseError || error{OutOfMemory})!*u32 {\n    return 0;\n}\n";
    let want = "fn parseDoubled(\n    allocator: Allocator,\n    text: []const u8,\n    extra: usize,\n) (ParseError || error{OutOfMemory})!*u32 {\n    return 0;\n}\n";
    expect(src, want);
}

#[test]
fn wraps_a_long_init_one_field_per_line() {
    let src = "const exe = b.addExecutable(.{ .name = name, .root_source = b.path(\"x\"), .target = target, .optimize = optimize });\n";
    let want = "const exe = b.addExecutable(.{\n    .name = name,\n    .root_source = b.path(\"x\"),\n    .target = target,\n    .optimize = optimize,\n});\n";
    expect(src, want);
}

#[test]
fn switch_with_block_arm_gets_trailing_comma() {
    let src = "fn f() void {\n    switch (x) {\n        .A => |info| {\n            g(info);\n        },\n        else => h(),\n    }\n}\n";
    expect(src, src);
}

#[test]
fn container_with_align_and_default() {
    let src = "const S = struct {\n    x: u32 align(4) = 0,\n    y: u8,\n};\n";
    expect(src, src);
}

#[test]
fn pointer_align_const_ordering() {
    expect(
        "const T = []align(16) const u8;\n",
        "const T = []align(16) const u8;\n",
    );
}

#[test]
fn examples_match_goldens() {
    let bless = std::env::var_os("K2_FMT_BLESS").is_some();
    for (name, src) in EXAMPLES {
        let formatted =
            format_source(src).unwrap_or_else(|d| panic!("{name} failed to format: {d:?}"));
        let path = golden_path(name);
        if bless {
            fs::write(&path, &formatted).expect("write golden");
            continue;
        }
        let expected = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display()));
        assert_eq!(
            formatted, expected,
            "{name} no longer matches its golden (run K2_FMT_BLESS=1 to update)"
        );
        // A golden, re-formatted, must be unchanged: idempotence on the fixed
        // point.
        let again = format_source(&expected).expect("golden must format");
        assert_eq!(
            again, expected,
            "golden {name} is not a formatter fixed point"
        );
    }
}
