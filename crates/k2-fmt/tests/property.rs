//! Property tests for the canonical formatter.
//!
//! These are the milestone-v0.3 acceptance gate. Over a hand-written corpus of
//! diverse k2 snippets — block-in-value-position, labeled blocks (as values and
//! as branch bodies, with `break :label`), `if`/`else` as a value both short and
//! long-enough-to-wrap, `while`/`for`/`switch` as values, long binary `++`/
//! arithmetic chains at the top level and as call arguments, deeply nested
//! calls/inits, and `struct`/`enum`/`union` declarations — plus the same
//! snippets with `//` and `///` comments injected into call-arg lists, fn
//! param lists, init/struct bodies (leading, trailing-same-line, and
//! own-line-between-elements), on block-opener lines, between `}` and `else`,
//! and at EOF, every snippet is asserted to satisfy FOUR invariants:
//!
//! 1. **VALID** — `fmt(src)` re-parses with zero error diagnostics (the
//!    formatter never emits unparseable output).
//! 2. **NO-CODE-LOSS** — `parse(fmt(src))` is structurally equal to `parse(src)`
//!    ignoring spans (compared via the spans-free S-expression form). Nothing is
//!    deleted, no meaning changes, labels are preserved.
//! 3. **IDEMPOTENT** — `fmt(fmt(src)) == fmt(src)`.
//! 4. **COMMENTS** — the multiset of comment texts (both `//` line comments and
//!    `///` doc comments) in `fmt(src)` equals that in `src`: none lost, none
//!    duplicated.

use k2_lexer::{tokenize_with_trivia, TriviaKind};
use k2_parse::{parse, to_sexpr};

/// Formats `src`, asserting it parses cleanly (the corpus is all valid k2).
fn fmt(src: &str) -> String {
    k2_fmt::format_source(src).unwrap_or_else(|d| {
        panic!("corpus snippet must format; parse errors: {d:?}\n--- src ---\n{src}")
    })
}

/// The sorted multiset of every comment's trimmed text in `src`, covering both
/// `//` line comments and `///` doc comments, recovered through the lexer's
/// trivia side channel (position-independent, so a relocated comment is still
/// counted — this checks loss/duplication, not placement).
fn comment_multiset(src: &str) -> Vec<String> {
    let (_toks, trivia) = tokenize_with_trivia(src);
    let mut out: Vec<String> = trivia
        .iter()
        .filter(|t| matches!(t.kind, TriviaKind::LineComment | TriviaKind::DocComment))
        .map(|t| t.text.trim_end().to_string())
        .collect();
    out.sort();
    out
}

/// Asserts the four formatter invariants on one corpus snippet, attributing any
/// failure to the snippet that triggered it.
fn assert_invariants(name: &str, src: &str) {
    // The snippet itself must be valid k2 (the corpus is hand-written; a typo
    // here would otherwise masquerade as a formatter bug).
    let parsed = parse(src);
    assert!(
        parsed.is_ok(),
        "[{name}] corpus snippet does not parse cleanly:\n{src}\ndiagnostics: {:?}",
        parsed.diagnostics
    );

    let once = fmt(src);

    // 1. VALID: the output re-parses with zero errors.
    let reparsed = parse(&once);
    assert!(
        reparsed.is_ok(),
        "[{name}] fmt output does not re-parse:\n--- src ---\n{src}\n--- fmt ---\n{once}\ndiagnostics: {:?}",
        reparsed.diagnostics
    );

    // 2. NO-CODE-LOSS: structural (spans-free) AST equality.
    assert_eq!(
        to_sexpr(&parsed.file),
        to_sexpr(&reparsed.file),
        "[{name}] AST changed when formatting (code loss / meaning change):\n--- src ---\n{src}\n--- fmt ---\n{once}"
    );

    // 3. IDEMPOTENT: a second pass is a no-op.
    let twice = fmt(&once);
    assert_eq!(
        once, twice,
        "[{name}] fmt is not idempotent:\n--- pass 1 ---\n{once}\n--- pass 2 ---\n{twice}"
    );

    // 4. COMMENTS: the comment multiset is preserved exactly.
    assert_eq!(
        comment_multiset(src),
        comment_multiset(&once),
        "[{name}] comment multiset changed (loss/duplication):\n--- src ---\n{src}\n--- fmt ---\n{once}"
    );
}

/// The corpus: `(name, source)` pairs. Every source must be valid k2 and end in
/// a newline. Comment-injected variants live alongside their bare forms so a
/// failure points straight at the construct + comment placement that broke.
const CORPUS: &[(&str, &str)] = &[
    // ---- block in value position ----
    (
        "block-value-labeled",
        "const x = blk: {\n    const y = 1;\n    break :blk y;\n};\n",
    ),
    ("block-value-bare", "const a = {\n    break 1;\n};\n"),
    (
        "block-value-return",
        "fn f() u8 {\n    return blk: {\n        break :blk 1;\n    };\n}\n",
    ),
    (
        "block-value-assign",
        "fn f() void {\n    x = blk: {\n        break :blk 1;\n    };\n}\n",
    ),
    (
        "block-statement-labeled",
        "test \"t\" {\n    blk: {\n        foo();\n    }\n}\n",
    ),
    (
        "block-value-with-comment",
        "const x = blk: {\n    // inner\n    const y = 1;\n    break :blk y;\n};\n",
    ),
    (
        "block-value-trailing-comment",
        "const x = blk: {\n    const y = 1; // keep\n    break :blk y;\n};\n",
    ),
    // ---- labeled block as a branch body (with break :label) ----
    (
        "labeled-block-then",
        "const x = if (a) lbl: {\n    break :lbl 1;\n} else 2;\n",
    ),
    (
        "labeled-block-else",
        "const x = if (a) 1 else lbl: {\n    break :lbl 2;\n};\n",
    ),
    (
        "labeled-block-switch-arm",
        "fn f() u8 {\n    return switch (x) {\n        0 => blk: {\n            break :blk 1;\n        },\n        else => 2,\n    };\n}\n",
    ),
    // ---- if/else as a value: short and long-enough-to-wrap ----
    ("if-value-short", "const x = if (cond) a else b;\n"),
    (
        "if-value-capture",
        "fn f() void {\n    const x = if (opt) |v| v else 0;\n}\n",
    ),
    (
        "if-value-else-capture",
        "fn f() void {\n    const x = if (a) b else |e| c;\n}\n",
    ),
    (
        "if-value-long",
        "const x = if (cond) aaaaaaaaaaaaaaaaaaaaaaaaaa else bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb;\n",
    ),
    (
        "if-value-braced",
        "fn f() u8 {\n    return if (a) {\n        break 1;\n    } else {\n        break 2;\n    };\n}\n",
    ),
    (
        "if-else-if-chain",
        "const x = if (a) 1 else if (b) 2 else 3;\n",
    ),
    (
        "if-value-comment-before-else",
        "fn f() void {\n    if (a) {\n        x();\n    }\n    // before else\n    else {\n        y();\n    }\n}\n",
    ),
    (
        "if-value-comment-on-opener",
        "const x = if (a) // then comment is tricky\n    b\nelse\n    c;\n",
    ),
    (
        "if-stmt-comment-on-then-opener",
        "fn f() void {\n    if (a) // s\n        x();\n}\n",
    ),
    // ---- while / for / switch as values ----
    (
        "while-value-long",
        "const x = while (longcondxxxxxxxxxxxx) bodyyyyyyyyyyyyyyyyyyyyy else fallbackkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk;\n",
    ),
    (
        "for-value",
        "fn f() void {\n    const s = for (items) |it| {\n        break it;\n    } else default;\n}\n",
    ),
    (
        "while-else-capture-stmt",
        "fn f() void {\n    while (a) |x| {\n        use(x);\n    } else |e| {\n        handle(e);\n    }\n}\n",
    ),
    (
        "for-capture-value",
        "fn f() void {\n    const x = for (xs) |y| y else 0;\n}\n",
    ),
    (
        "labeled-while-value",
        "fn f() void {\n    const x = outer: while (a) {\n        break :outer 1;\n    };\n}\n",
    ),
    (
        "inline-while-stmt",
        "fn f() void {\n    inline while (a) {\n        b();\n    }\n}\n",
    ),
    (
        "switch-value",
        "fn f() u8 {\n    return switch (x) {\n        0 => 1,\n        1...9 => 2,\n        else => 3,\n    };\n}\n",
    ),
    (
        "switch-value-comment",
        "fn f() u8 {\n    return switch (x) {\n        // zero\n        0 => 1,\n        else => 2, // default\n    };\n}\n",
    ),
    // ---- long binary chains: top level and as call args ----
    (
        "binary-chain-top",
        "const x = aaaaaaaaaa ++ bbbbbbbbbb ++ cccccccccc ++ dddddddddd ++ eeeeeeeeee ++ ffffffffff ++ gggggggggg ++ hhhhhhhhhh;\n",
    ),
    (
        "binary-chain-arith-top",
        "const x = aaaaaaaaaa + bbbbbbbbbb + cccccccccc + dddddddddd + eeeeeeeeee + ffffffffff + gggggggggg + hhhhhhhhhh + iiiiiiiiii;\n",
    ),
    (
        "binary-chain-call-arg",
        "const x = f(aaaaaaaaaa ++ bbbbbbbbbb ++ cccccccccc ++ dddddddddd ++ eeeeeeeeee ++ ffffffffff ++ gggggggggg ++ hhhhhhhhhh);\n",
    ),
    // ---- deeply nested calls / inits ----
    (
        "nested-call",
        "const x = outer(inner(deep(a, b), c), d);\n",
    ),
    (
        "nested-call-long",
        "const x = outer(innerrrrrrrrrrrrrrrr(deepppppppppppppppp(aaaaaaaaaaaa, bbbbbbbbbbbb), cccccccccccc), ddddddddddddd);\n",
    ),
    (
        "nested-init",
        "const x = .{ .a = .{ .b = 1, .c = 2 }, .d = 3 };\n",
    ),
    (
        "nested-init-long",
        "const x = .{ .aaaaaaaaaa = .{ .bbbbbbbbbb = 1, .cccccccccc = 2 }, .dddddddddd = 3, .eeeeeeeeee = 4, .ffffffffff = 5 };\n",
    ),
    (
        "tuple-init-long",
        "const x = .{ firstElementHereXXXX, secondElementHereXX, thirdElementHereXXXX, fourthElementHereXXXXX, fifthElementHereXXXXXX };\n",
    ),
    // ---- struct / enum / union decls ----
    (
        "struct-decl",
        "const Point = struct {\n    x: i32,\n    y: i32,\n};\n",
    ),
    ("enum-decl", "const E = enum(u8) {\n    A,\n    B,\n    C,\n};\n"),
    (
        "union-decl",
        "const U = union(enum) {\n    a: u8,\n    b: u16,\n};\n",
    ),
    (
        "struct-with-fn",
        "const S = struct {\n    x: i32,\n\n    pub fn get(self: S) i32 {\n        return self.x;\n    }\n};\n",
    ),
    // ---- comments injected into call arg lists ----
    (
        "call-arg-trailing-comment",
        "const x = f(\n    a, // first\n    b, // second\n    c,\n);\n",
    ),
    (
        "call-arg-own-line-between",
        "const x = f(\n    aaaaaaaaaaaaaaaaaaaa,\n    // between args\n    bbbbbbbbbbbbbbbbbbbb,\n);\n",
    ),
    (
        "call-arg-comment-before-close",
        "const x = f(\n    aaaaaaaaaaaaaaaaaaaa,\n    bbbbbbbbbbbbbbbbbbbb,\n    // last comment\n);\n",
    ),
    (
        "call-arg-leading-comment",
        "const x = f(\n    // lead a\n    aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,\n    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb,\n);\n",
    ),
    (
        "call-arg-interior-comment-collapses",
        "const x = f(a, b, // c\n    c);\n",
    ),
    (
        "builtin-arg-comment",
        "const x = @b(\n    arg_one_xx, // c\n    arg_two_xx,\n);\n",
    ),
    (
        "nested-call-deep-comment",
        "const x = outer(\n    inner(\n        aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, // deep\n        bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb,\n    ),\n    ccccccccccccccccccccccccccccccc,\n);\n",
    ),
    // ---- comments injected into fn param lists ----
    (
        "param-trailing-comment",
        "fn g(\n    a: u8, // p1\n    b: u8, // p2\n) void {\n    body();\n}\n",
    ),
    (
        "param-leading-comment",
        "fn g(\n    // first param\n    a: u8,\n    b: u8,\n) void {}\n",
    ),
    (
        "param-proto-comment",
        "extern fn g(\n    a: u8, // c\n    b: u8,\n) void;\n",
    ),
    // ---- comments injected into init / struct bodies ----
    (
        "init-field-leading-comment",
        "const x = .{\n    // a field\n    .aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1,\n    .bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb = 2,\n};\n",
    ),
    (
        "init-field-trailing-comment",
        "const x = .{\n    .aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1, // one\n    .bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb = 2, // two\n};\n",
    ),
    (
        "init-field-own-line-comment",
        "const x = .{\n    .aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1,\n    // between\n    .bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb = 2,\n};\n",
    ),
    (
        "struct-field-comments",
        "const S = struct {\n    // x coord\n    x: i32,\n    y: i32, // y coord\n};\n",
    ),
    (
        "struct-field-doc-comments",
        "const S = struct {\n    /// the x coordinate\n    x: i32,\n    /// the y coordinate\n    y: i32,\n};\n",
    ),
    // ---- comment on a block opener line ----
    (
        "block-opener-comment-fn",
        "fn f() void { // sig\n    return;\n}\n",
    ),
    (
        "block-opener-comment-bare",
        "test \"t\" {\n    { // inner block\n        return;\n    }\n}\n",
    ),
    (
        "block-opener-comment-struct",
        "const S = struct { // fields\n    a: u8,\n    b: u8,\n};\n",
    ),
    (
        "block-opener-comment-if",
        "fn f() void {\n    if (a) { // then\n        x();\n    }\n}\n",
    ),
    (
        "block-opener-comment-switch-arm",
        "fn f() void {\n    switch (x) {\n        0 => { // zero\n            a();\n        },\n        else => {},\n    }\n}\n",
    ),
    (
        "container-close-comment",
        "const S = struct {\n    a: u8,\n}; // a struct\n",
    ),
    (
        "init-close-comment",
        "fn f() void {\n    const x = .{\n        .aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1,\n        .bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb = 2,\n    }; // an init\n}\n",
    ),
    (
        "empty-then-trailing-comment",
        "fn f() void {\n    if (x) {} // empty then\n    else {}\n}\n",
    ),
    (
        "tuple-mid-trailing-comment",
        "const x = .{\n    firstElementHereXXXXXXXXXX,\n    secondElementHereXXXXXXXX, // mid\n    thirdElementHereXXXXXXXXXX,\n};\n",
    ),
    // ---- comment between `}` and `else` ----
    (
        "comment-between-brace-and-else",
        "fn f() void {\n    if (a) {\n        x();\n    }\n    // documents else\n    else {\n        y();\n    }\n}\n",
    ),
    // ---- comment at EOF ----
    ("eof-comment", "const x = 1;\n// trailing file comment\n"),
    (
        "eof-doc-orphan",
        "const x = 1;\n// a\n// b\n",
    ),
    // ---- doc comments on items ----
    (
        "doc-comment-fn",
        "/// does a thing\npub fn f() void {}\n",
    ),
    (
        "doc-comment-const",
        "/// the answer\nconst x = 42;\n",
    ),
    // ---- mixed / misc ----
    (
        "catch-block-value",
        "fn f() void {\n    const x = mayFail() catch |err| {\n        handle(err);\n    };\n}\n",
    ),
    (
        "defer-block",
        "fn f() void {\n    defer {\n        cleanup();\n    }\n}\n",
    ),
    (
        "long-fn-signature",
        "pub fn doSomethingWithAVeryLongName(first_argument: u32, second_argument: u32, third_argument: u32) void {}\n",
    ),
];

#[test]
fn corpus_satisfies_all_four_invariants() {
    for (name, src) in CORPUS {
        assert_invariants(name, src);
    }
}

#[test]
fn corpus_is_nonempty_and_diverse() {
    // A guard so the gate cannot be silently emptied: the corpus must stay large
    // and cover the catastrophic-bug shapes by name.
    assert!(
        CORPUS.len() >= 40,
        "corpus shrank below the acceptance floor"
    );
    for needle in [
        "block-value-labeled",
        "labeled-block-then",
        "if-value-long",
        "call-arg-interior-comment-collapses",
        "binary-chain-top",
    ] {
        assert!(
            CORPUS.iter().any(|(n, _)| *n == needle),
            "corpus lost required snippet {needle}"
        );
    }
}
