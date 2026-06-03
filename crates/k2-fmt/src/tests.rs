//! Tests for the canonical formatter.
//!
//! Four properties are exercised across a corpus of the six `examples/*.k2`
//! files plus targeted micro-cases:
//!
//! * **idempotence** — `fmt(fmt(x)) == fmt(x)`;
//! * **AST round-trip** — `to_sexpr(parse(x)) == to_sexpr(parse(fmt(x)))`, i.e.
//!   formatting changes only whitespace/comments/parens, never structure;
//! * **comment preservation** — the multiset of `//`/`///` comment lines is
//!   unchanged by formatting;
//! * **golden output** — the example corpus formats to a stable, reviewed form.

use crate::format_source;
use k2_parse::{parse, to_sexpr};

/// The six bundled example programs, embedded so tests stay offline.
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

/// Formats `src`, asserting it parses cleanly.
fn fmt(src: &str) -> String {
    format_source(src).unwrap_or_else(|d| panic!("expected clean parse, got {d:?}"))
}

/// The sorted multiset of comment lines (`//` and `///`) in `src`.
fn comment_texts(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            out.push(t.trim_end().to_string());
        }
    }
    out.sort();
    out
}

// ---- micro-cases: every spacing/break rule -------------------------------

#[test]
fn formats_a_const() {
    assert_eq!(fmt("const  x=1 ;\n"), "const x = 1;\n");
}

#[test]
fn normalizes_binary_spacing_and_drops_redundant_parens() {
    assert_eq!(fmt("const x = ((a))+b*c;\n"), "const x = a + b * c;\n");
    assert_eq!(fmt("const x = (a+b)*c;\n"), "const x = (a + b) * c;\n");
    assert_eq!(fmt("const x = a-(b-c);\n"), "const x = a - (b - c);\n");
}

#[test]
fn empty_bodies_collapse() {
    assert_eq!(fmt("fn f() void {}\n"), "fn f() void {}\n");
    assert_eq!(fmt("test \"t\" {}\n"), "test \"t\" {}\n");
}

#[test]
fn postfix_type_spellings() {
    // The prefix type-constructors (`?`, `*`, `[]`, `[N]`, `!T`) parse as the
    // value of a `const`; the infix `E!T` error-union only parses in true type
    // position (a parameter type), so it is exercised separately below.
    assert_eq!(fmt("const T = ?u8;\n"), "const T = ?u8;\n");
    assert_eq!(fmt("const T = *const u8;\n"), "const T = *const u8;\n");
    assert_eq!(
        fmt("const T = *align(16) const u8;\n"),
        "const T = *align(16) const u8;\n"
    );
    assert_eq!(fmt("const T = []const u8;\n"), "const T = []const u8;\n");
    assert_eq!(fmt("const T = [4]u8;\n"), "const T = [4]u8;\n");
    assert_eq!(fmt("const T = !void;\n"), "const T = !void;\n");
}

#[test]
fn error_union_in_type_position() {
    // `E!T` and `(A || B)!T` are type-position forms; use a parameter type.
    assert_eq!(
        fmt("fn f(x: ParseError!u32) void {}\n"),
        "fn f(x: ParseError!u32) void {}\n"
    );
    assert_eq!(
        fmt("fn f(x: (A || B)!u32) void {}\n"),
        "fn f(x: (A || B)!u32) void {}\n"
    );
}

#[test]
fn error_set_padding_by_arity() {
    assert_eq!(fmt("const E = error{};\n"), "const E = error{};\n");
    assert_eq!(fmt("const E = error{One};\n"), "const E = error{One};\n");
    assert_eq!(
        fmt("const E = error{A, B};\n"),
        "const E = error{ A, B };\n"
    );
}

#[test]
fn init_padding_by_arity() {
    // A single tuple element is tight; multiple are padded; empty has no pad.
    assert_eq!(fmt("const x = .{};\n"), "const x = .{};\n");
    assert_eq!(fmt("const x = .{a};\n"), "const x = .{a};\n");
    assert_eq!(fmt("const x = .{a, b};\n"), "const x = .{ a, b };\n");
    // Named-field inits always pad.
    assert_eq!(fmt("const x = .{.a=1};\n"), "const x = .{ .a = 1 };\n");
}

#[test]
fn braceless_if_body_stays_inline() {
    let src = "fn f() void {\n    if (a) return;\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn braced_single_statement_stays_braced() {
    let src = "fn f() void {\n    if (a) {\n        return;\n    }\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn switch_arms_get_trailing_commas() {
    let src =
        "fn f() u8 {\n    return switch (x) {\n        0 => 1,\n        else => 2,\n    };\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn inclusive_switch_range_is_tight() {
    let src =
        "fn f() u8 {\n    return switch (x) {\n        0...9 => 1,\n        else => 2,\n    };\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn multiline_string_is_verbatim() {
    let src = "const s =\n    \\\\line one\n    \\\\line two\n;\n";
    let out = fmt(src);
    assert!(out.contains("\\\\line one"));
    assert!(out.contains("\\\\line two"));
}

#[test]
fn integer_literals_are_verbatim() {
    assert_eq!(fmt("const x = 0xFF_FF;\n"), "const x = 0xFF_FF;\n");
    assert_eq!(fmt("const x = 1_000_000;\n"), "const x = 1_000_000;\n");
}

// ---- comment micro-cases -------------------------------------------------

#[test]
fn leading_comment_preserved() {
    let src = "// lead\nconst x = 1;\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn trailing_same_line_comment_preserved() {
    let src = "const x = 1; // trail\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn dangling_comment_in_empty_block_kept() {
    let src = "fn f() void {\n    // only a comment\n}\n";
    let out = fmt(src);
    assert!(out.contains("// only a comment"));
    // The block is NOT collapsed to `{}` because it holds a comment.
    assert!(!out.contains("{}"));
}

#[test]
fn doc_comment_on_fn_preserved() {
    let src = "/// doc line\npub fn f() void {}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn four_slash_line_comment_is_kept() {
    let src = "//// four slashes\nconst x = 1;\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn empty_file_formats_to_newline() {
    assert_eq!(fmt(""), "\n");
    assert_eq!(fmt("   \n\n"), "\n");
}

#[test]
fn file_of_only_comments_preserves_them() {
    let src = "// a\n// b\n";
    let out = fmt(src);
    assert!(out.contains("// a"));
    assert!(out.contains("// b"));
}

// ---- refusal on parse errors ---------------------------------------------

#[test]
fn refuses_to_format_broken_input() {
    let res = format_source("fn f( {\n");
    assert!(res.is_err(), "broken input must not format");
}

// ---- corpus-wide properties ----------------------------------------------

#[test]
fn examples_format_without_error() {
    for (name, src) in EXAMPLES {
        assert!(
            format_source(src).is_ok(),
            "{name} should format without parse errors"
        );
    }
}

#[test]
fn examples_are_idempotent() {
    for (name, src) in EXAMPLES {
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "fmt is not idempotent on {name}");
    }
}

#[test]
fn examples_round_trip_the_ast() {
    for (name, src) in EXAMPLES {
        let a = parse(src);
        assert!(a.is_ok(), "{name} must parse");
        let formatted = fmt(src);
        let b = parse(&formatted);
        assert!(b.is_ok(), "{name} must still parse after formatting");
        assert_eq!(
            to_sexpr(&a.file),
            to_sexpr(&b.file),
            "AST changed when formatting {name}"
        );
    }
}

#[test]
fn examples_preserve_every_comment() {
    for (name, src) in EXAMPLES {
        let formatted = fmt(src);
        assert_eq!(
            comment_texts(src),
            comment_texts(&formatted),
            "a comment was lost or altered when formatting {name}"
        );
    }
}

#[test]
fn committed_examples_are_canonical() {
    // The bundled examples are kept in canonical form (the milestone runs
    // `k2c fmt --write` across them), so formatting each one is the identity.
    // This guards against an example drifting out of canonical form.
    for (name, src) in EXAMPLES {
        assert_eq!(fmt(src), *src, "{name} is not in canonical form");
    }
}
