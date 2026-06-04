//! Golden + robustness tests for the diagnostic renderer.
//!
//! All tests run with `color = false` and `tab_width = 4` (the harness default),
//! so the asserted strings are the exact plain-text layout. The goal is to pin
//! the caret alignment under multi-byte and tab cases and to prove the renderer
//! never panics on adversarial input.

use super::*;
use k2_syntax::{RichDiagnostic, Span};

/// The renderer options used by every golden test.
fn opts() -> RenderOpts {
    RenderOpts {
        color: false,
        tab_width: 4,
    }
}

/// Builds a span over `[start, end)` char offsets on a given 1-based line/col.
fn span(start: u32, end: u32, line: u32, col: u32) -> Span {
    Span::new(start, end, line, col)
}

/// Locates the `^`-underline row that follows the source line containing
/// `needle`, returning the 0-based column of the first `^`.
fn caret_col_for(rendered: &str, needle: &str) -> usize {
    let lines: Vec<&str> = rendered.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if l.contains(needle) {
            // The next line with a `^` is the underline row.
            for cand in &lines[i + 1..] {
                if let Some(p) = cand.find('^') {
                    // Subtract the gutter `" | "` prefix length so the column is
                    // relative to the source text start.
                    let bar = cand.find('|').unwrap();
                    return p - (bar + 2);
                }
            }
        }
    }
    panic!("no caret row found after line containing {needle:?}\n{rendered}");
}

/// The display column of `needle`'s first char within `line` (tab=4).
fn token_display_col(line: &str, needle: &str) -> usize {
    let byte = line.find(needle).expect("needle in line");
    let char_col = line[..byte].chars().count();
    display_col(line, char_col, 4)
}

#[test]
fn type_mismatch_full_report() {
    let src = "const x: i32 = flag;\n";
    // `flag` is at char offset 15..19 on line 1; `i32` (annotation) at 9..12.
    let d = RichDiagnostic::error(span(15, 19, 1, 16), "expected `i32`, found `bool`")
        .primary_label("this is `bool`")
        .secondary(span(9, 12, 1, 10), "expected `i32` because of this")
        .help("convert with `@as(i32, …)` if a narrowing cast is intended");
    let r = render("bad.k2", src, &d, &opts());
    assert!(r.contains("error: expected `i32`, found `bool`"), "{r}");
    assert!(r.contains("--> bad.k2:1:16"), "{r}");
    assert!(r.contains("const x: i32 = flag;"), "{r}");
    assert!(r.contains("^^^^ this is `bool`"), "{r}");
    assert!(r.contains("--- "), "{r}"); // a secondary underline exists
    assert!(r.contains("= help: convert with `@as(i32, …)`"), "{r}");
    // The caret aligns under `flag`.
    let line = "const x: i32 = flag;";
    assert_eq!(
        caret_col_for(&r, "const x: i32"),
        token_display_col(line, "flag")
    );
}

#[test]
fn undeclared_with_help() {
    let src = "const y = zz;\n";
    let d = RichDiagnostic::error(span(10, 12, 1, 11), "use of undeclared identifier `zz`")
        .primary_label("not found in this scope")
        .help("a binding named `z` exists — did you mean it?");
    let r = render("u.k2", src, &d, &opts());
    assert!(r.contains("^^ not found in this scope"), "{r}");
    assert!(r.contains("= help: a binding named `z` exists"), "{r}");
}

#[test]
fn non_exhaustive_switch_note_and_help() {
    let src = "    switch (x) {}\n";
    // `switch` keyword at char 4..10.
    let d = RichDiagnostic::error(span(4, 10, 1, 5), "switch is not exhaustive")
        .primary_label("this switch does not cover all cases")
        .note("missing cases: `.B`, `.C`")
        .help("add the missing arm(s) or an `else =>` branch");
    let r = render("s.k2", src, &d, &opts());
    assert!(r.contains("= note: missing cases: `.B`, `.C`"), "{r}");
    assert!(
        r.contains("= help: add the missing arm(s) or an `else =>` branch"),
        "{r}"
    );
    // The caret sits under `switch`.
    assert_eq!(caret_col_for(&r, "switch (x)"), 4);
}

#[test]
fn parse_expected_zero_width_caret() {
    let src = "const x = ;\n";
    // Zero-width span at the `;` position (char 10).
    let d = RichDiagnostic::error(span(10, 10, 1, 11), "expected expression, found `;`")
        .primary_label("expected expression here")
        .note("while parsing a `const` initializer");
    let r = render("p.k2", src, &d, &opts());
    // A zero-width span still draws a single caret.
    assert!(r.contains("^ expected expression here"), "{r}");
    assert!(
        r.contains("= note: while parsing a `const` initializer"),
        "{r}"
    );
    assert_eq!(caret_col_for(&r, "const x ="), 10);
}

#[test]
fn multibyte_caret_alignment() {
    // `café` precedes the offending `x`. `é` is 2 UTF-8 bytes but 1 display col.
    let src = "const café = 1; y = caf;\n";
    let chars: Vec<char> = src.chars().collect();
    // Find char offset of `caf` (the second one, the undeclared use).
    let s: String = chars.iter().collect();
    let byte = s.rfind("caf").unwrap();
    let char_off = s[..byte].chars().count() as u32;
    let d = RichDiagnostic::error(
        span(char_off, char_off + 3, 1, char_off + 1),
        "use of undeclared identifier `caf`",
    )
    .primary_label("not found");
    let r = render("m.k2", src, &d, &opts());
    let line = "const café = 1; y = caf;";
    // The caret must land under the *display* column of `caf`, not its byte pos.
    assert_eq!(
        caret_col_for(&r, "const café"),
        token_display_col(line, "caf;").saturating_sub(0)
    );
    // Sanity: display col != byte offset for this line (é is multi-byte).
    assert_ne!(token_display_col(line, "caf;"), line.find("caf;").unwrap());
}

#[test]
fn cjk_wide_caret_alignment() {
    // A width-2 CJK identifier precedes the token.
    let src = "var 変数 = zz;\n";
    let s = src.to_string();
    let byte = s.find("zz").unwrap();
    let char_off = s[..byte].chars().count() as u32;
    let d = RichDiagnostic::error(
        span(char_off, char_off + 2, 1, char_off + 1),
        "undeclared `zz`",
    )
    .primary_label("here");
    let r = render("c.k2", src, &d, &opts());
    let line = "var 変数 = zz;";
    // Each CJK char is width-2, so the display column is larger than the char
    // offset.
    let dc = token_display_col(line, "zz");
    assert_eq!(caret_col_for(&r, "var 変数"), dc);
    assert!(dc > line.chars().take_while(|c| *c != 'z').count() - 2);
}

#[test]
fn tab_caret_alignment_width4() {
    // A real tab indents the line; the caret must reproduce the tab so it lands
    // under the token at tab width 4.
    let src = "\tconst x = zz;\n";
    let s = src.to_string();
    let byte = s.find("zz").unwrap();
    let char_off = s[..byte].chars().count() as u32;
    let d = RichDiagnostic::error(
        span(char_off, char_off + 2, 1, char_off + 1),
        "undeclared `zz`",
    )
    .primary_label("here");
    let r = render("t.k2", src, &d, &opts());
    // The underline row must begin with a verbatim tab (copied from source).
    let underline = r.lines().find(|l| l.contains('^')).expect("underline row");
    let after_bar = &underline[underline.find('|').unwrap() + 2..];
    assert!(
        after_bar.starts_with('\t'),
        "underline must copy the leading tab verbatim: {after_bar:?}"
    );
}

#[test]
fn tab_caret_alignment_width8() {
    let src = "\tx = zz;\n";
    let s = src.to_string();
    let byte = s.find("zz").unwrap();
    let char_off = s[..byte].chars().count() as u32;
    let d = RichDiagnostic::error(
        span(char_off, char_off + 2, 1, char_off + 1),
        "undeclared `zz`",
    )
    .primary_label("here");
    let o = RenderOpts {
        color: false,
        tab_width: 8,
    };
    let r = render("t8.k2", src, &d, &o);
    let underline = r.lines().find(|l| l.contains('^')).unwrap();
    let after_bar = &underline[underline.find('|').unwrap() + 2..];
    // Still copies the verbatim tab; alignment is tab-width-agnostic.
    assert!(after_bar.starts_with('\t'), "{after_bar:?}");
}

#[test]
fn multiline_span_rail() {
    let src = "const x = foo(\n    a, b);\n";
    // Span from `foo(` on line 1 to `)` on line 2.
    let s = src.to_string();
    let start = s.find("foo").unwrap() as u32;
    let end = (s.find(");").unwrap() + 1) as u32;
    let d = RichDiagnostic::error(span(start, end, 1, 11), "this whole call")
        .primary_label("the call spans lines");
    let r = render("ml.k2", src, &d, &opts());
    // Opening rail line ends with a `^`; closing rail begins with `|`.
    assert!(r.lines().any(|l| l.trim_end().ends_with('^')), "{r}");
    assert!(
        r.lines()
            .any(|l| l.contains("|_") || l.contains("|^") || l.contains("|")),
        "{r}"
    );
}

#[test]
fn multiline_closing_rail_caret_sits_under_last_char() {
    // The closing `^` must land under the span's LAST char (inclusive), not one
    // (or two) cells past it. Mirrors the CLI `g(\n  1,\n  2,\n  );` repro: the
    // span's last char is `)` on the closing line; the caret must sit on it.
    let src = "const x = g(\n    1,\n    2,\n  );\n";
    let s = src.to_string();
    let start = s.find("g(").unwrap() as u32;
    // Inclusive last char is the `)` on line 4; the exclusive span end is one past.
    let close_byte = s.rfind(')').unwrap();
    let end = (close_byte + 1) as u32; // exclusive end
    let d = RichDiagnostic::error(span(start, end, 1, 11), "spans lines").primary_label("the call");
    let r = render("ml2.k2", src, &d, &opts());

    let lines: Vec<&str> = r.lines().collect();
    // Find the printed source row for line 4 (`  );`), then its following rail row.
    let src_row_idx = lines
        .iter()
        .position(|l| l.contains("4 | ") && l.contains(')'))
        .expect("line-4 source row");
    let rail = lines[src_row_idx + 1];
    // Column of the `^` within the rail, relative to source-text start (after the
    // gutter `" | "`).
    let bar = rail.find('|').unwrap();
    let caret = rail.rfind('^').expect("closing caret");
    let caret_col = caret - (bar + 2);
    // The target column is the display column of the INCLUSIVE last char (`)`),
    // which on the line `  );` sits at display column 2.
    let line4 = "  );"; // line 4 text (no trailing newline)
    let last_char_col = line4.find(')').unwrap(); // char-col of `)` == 2
    assert_eq!(
        caret_col,
        display_col(line4, last_char_col, 4),
        "closing caret not under the last char\n{r}"
    );
    assert_eq!(caret_col, 2, "{r}");
}

#[test]
fn multiline_closing_rail_caret_under_paren_indented() {
    // The exact CLI repro line shape `    );` — `)` at display col 4. The closing
    // `^` must be at col 4 (under `)`), not col 6 (the old +2 bug).
    let src = "const x: i32 = g(\n    1,\n    2,\n    );\n";
    let s = src.to_string();
    let start = s.find("g(").unwrap() as u32;
    let close_byte = s.rfind(')').unwrap();
    let end = (close_byte + 1) as u32;
    let d =
        RichDiagnostic::error(span(start, end, 1, 16), "this is `bool`").primary_label("the call");
    let r = render("ml3.k2", src, &d, &opts());
    let lines: Vec<&str> = r.lines().collect();
    let src_row_idx = lines
        .iter()
        .position(|l| l.contains(" | ") && l.trim_end().ends_with(");"))
        .expect("the `    );` source row");
    let rail = lines[src_row_idx + 1];
    let bar = rail.find('|').unwrap();
    let caret = rail.rfind('^').expect("closing caret");
    let caret_col = caret - (bar + 2);
    let line = "    );";
    // `)` is the inclusive last char at char-col 4; its display col is 4.
    assert_eq!(caret_col, display_col(line, 4, 4), "{r}");
    assert_eq!(caret_col, 4, "{r}");
}

#[test]
fn locator_header_agrees_with_snippet_line() {
    // The span's `line`/`col` fields are deliberately WRONG (they claim line 595,
    // col 1 — as an EOF-into-std span would) while the offset (12) resolves to
    // line 2 of the user source. The locator must follow the resolved offset, so
    // the header line matches the snippet line — not the bogus span field.
    let src = "fn f() {\nbad\n";
    // Offset 12 is on line 2 (`bad`), char-col 3 -> col 4 (1-based).
    let d = RichDiagnostic::error(span(11, 11, 595, 1), "unexpected end of input")
        .primary_label("here");
    let r = render("x.k2", src, &d, &opts());
    // The header must NOT show the phantom line 595.
    assert!(!r.contains(":595:"), "leaked phantom line:\n{r}");
    // Header line == the snippet's line number (2).
    assert!(
        r.contains("--> x.k2:2:"),
        "header not from resolved offset:\n{r}"
    );
    // And the snippet shows line 2.
    assert!(r.contains("2 | bad"), "{r}");
}

// ---- Robustness: never panic on adversarial input ----------------------

#[test]
fn empty_file_does_not_panic() {
    let d = RichDiagnostic::error(span(0, 0, 1, 1), "empty");
    let r = render("e.k2", "", &d, &opts());
    assert!(r.contains("error: empty"));
    assert!(r.contains("--> e.k2:1:1"));
}

#[test]
fn span_past_eof_does_not_panic() {
    let src = "abc";
    let d = RichDiagnostic::error(span(100, 200, 1, 101), "past end").primary_label("x");
    let r = render("p.k2", src, &d, &opts());
    assert!(r.contains("error: past end"));
    assert!(r.contains("abc"));
}

#[test]
fn end_before_start_swapped_or_clamped() {
    let src = "hello world\n";
    let d = RichDiagnostic::error(span(8, 3, 1, 9), "weird").primary_label("x");
    // Must not panic; clamps end up to start.
    let r = render("w.k2", src, &d, &opts());
    assert!(r.contains("error: weird"));
}

#[test]
fn single_newline_file() {
    let d = RichDiagnostic::error(span(0, 1, 1, 1), "nl").primary_label("x");
    let r = render("n.k2", "\n", &d, &opts());
    assert!(r.contains("error: nl"));
}

#[test]
fn huge_line_is_truncated_not_unbounded() {
    let big: String = "a".repeat(100_000);
    let src = format!("{big}\n");
    let d = RichDiagnostic::error(span(50_000, 50_004, 1, 50_001), "huge").primary_label("x");
    let r = render("h.k2", &src, &d, &opts());
    assert!(r.contains("error: huge"));
    // The rendered source line is bounded.
    let src_line = r.lines().find(|l| l.contains("aaaa")).unwrap();
    assert!(src_line.chars().count() < 1000, "line not truncated");
    // The underline (caret) row is ALSO bounded: it must not stretch out to the
    // span's raw display column (50 000+). Allow MAX_LINE_DISPLAY + gutter + a
    // little slack.
    let underline = r.lines().find(|l| l.contains('^')).expect("caret row");
    assert!(
        underline.chars().count() < MAX_LINE_DISPLAY + 32,
        "underline row not bounded: {} cells",
        underline.chars().count()
    );
}

#[test]
fn far_out_caret_on_long_line_collapses_to_marker() {
    // A span far past the truncation boundary (under the `…`) must render a
    // single, bounded caret at the truncation point — never a giant pad with an
    // off-screen `^`. (Regression for the dangling-caret long-line finding.)
    let big: String = "a".repeat(100_000);
    let src = format!("{big}\n");
    // The offending token starts at display col ~90 000, well past MAX_LINE_DISPLAY.
    let d = RichDiagnostic::error(span(90_000, 90_007, 1, 90_001), "far out").primary_label("here");
    let r = render("far.k2", &src, &d, &opts());
    let underline = r.lines().find(|l| l.contains('^')).expect("caret row");
    // Bounded row.
    assert!(
        underline.chars().count() < MAX_LINE_DISPLAY + 32,
        "underline not bounded: {} cells",
        underline.chars().count()
    );
    // Exactly one caret (collapsed marker), at the truncation boundary.
    assert_eq!(underline.matches('^').count(), 1, "{r}");
    let bar = underline.find('|').unwrap();
    let caret = underline.find('^').unwrap();
    assert_eq!(
        caret - (bar + 2),
        MAX_LINE_DISPLAY,
        "marker not at `…` column"
    );
}

#[test]
fn tab_only_line() {
    let src = "\t\t\t\n";
    let d = RichDiagnostic::error(span(1, 2, 1, 2), "tabs").primary_label("x");
    let r = render("tab.k2", src, &d, &opts());
    assert!(r.contains("error: tabs"));
}

#[test]
fn combining_marks_and_emoji_zero_and_wide_widths() {
    // `e` + combining acute (zero width) then a ZWJ emoji sequence.
    let src = "let e\u{0301} = 🧑\u{200d}🚀;\n";
    let d = RichDiagnostic::error(span(0, 3, 1, 1), "wat").primary_label("x");
    let r = render("z.k2", src, &d, &opts());
    assert!(r.contains("error: wat"));
}

#[test]
fn fuzz_random_spans_never_panic() {
    // Deterministic LCG over a handful of sources, feeding wild spans.
    let sources = [
        "",
        "\n",
        "x",
        "const café = 1;\n\tvar 変数 = 2;\nlast line no newline",
        "🧑\u{200d}🚀 spans 🎉 and more 漢字 text\n",
        &"a".repeat(5000),
    ];
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for src in sources {
        let n = src.chars().count() as u32;
        for _ in 0..2000 {
            let a = (next() % (n as u64 + 8)) as u32;
            let b = (next() % (n as u64 + 8)) as u32;
            let line = (next() % 4) as u32;
            let col = (next() % 80) as u32;
            let d = RichDiagnostic::error(span(a, b, line, col), "fuzz")
                .primary_label("x")
                .secondary(span(b, a, line + 1, col), "y")
                .note("n");
            let r = render("f.k2", src, &d, &opts());
            assert!(r.contains("error: fuzz"));
        }
    }
}

#[test]
fn color_mode_wraps_with_ansi() {
    let src = "const x = zz;\n";
    let d = RichDiagnostic::error(span(10, 12, 1, 11), "bad").primary_label("here");
    let o = RenderOpts {
        color: true,
        tab_width: 4,
    };
    let r = render("col.k2", src, &d, &o);
    assert!(
        r.contains("\x1b[31m"),
        "expected red escape in colored output"
    );
    assert!(r.contains("\x1b[0m"), "expected reset escape");
    // Plain mode has no escapes.
    let plain = render("col.k2", src, &d, &opts());
    assert!(!plain.contains('\x1b'));
}

#[test]
fn emit_rich_orders_by_span_and_counts_errors() {
    let src = "aaa bbb ccc\n";
    let diags = vec![
        RichDiagnostic::error(span(8, 11, 1, 9), "third"),
        RichDiagnostic::error(span(0, 3, 1, 1), "first"),
        RichDiagnostic::warning(span(4, 7, 1, 5), "second"),
    ];
    let mut buf: Vec<u8> = Vec::new();
    let errs = emit_rich(&mut buf, "o.k2", src, &diags, &opts());
    assert_eq!(errs, 2);
    let s = String::from_utf8(buf).unwrap();
    let fi = s.find("first").unwrap();
    let se = s.find("second").unwrap();
    let th = s.find("third").unwrap();
    assert!(fi < se && se < th, "diagnostics not in source order:\n{s}");
}
