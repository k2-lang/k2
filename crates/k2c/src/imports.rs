//! Import discovery + textual rewriting shared by the single-file and multi-file
//! compile paths.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! Two operations live here:
//!
//! * [`path_imports`] enumerates every `@import("...")` string in a parsed file
//!   (both top-level `const x = @import(...)` and any nested ones), so the
//!   multi-file driver can discover the module graph.
//! * [`rewrite_import_strings`] re-points each `@import("X")` in a file's *source
//!   text* to a bare namespace identifier, by **byte substitution in place** —
//!   so comments, formatting, and spans are preserved and the rewritten file
//!   still parses identically. This is the textual analogue of the in-AST
//!   `@import("std")` rewrite the driver already performs.

use k2_syntax::{Expr, Item, SourceFile, Stmt};

/// Collects every `@import("...")` literal string (quotes stripped) reachable in
/// `file` — top-level consts and nested expressions alike — in source order.
pub fn path_imports(file: &SourceFile) -> Vec<String> {
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    out
}

/// Collects imports from one item.
fn collect_item(item: &Item, out: &mut Vec<String>) {
    match item {
        Item::Const { value, .. } => collect_expr(value, out),
        Item::Var { value, .. } => {
            if let Some(v) = value {
                collect_expr(v, out);
            }
        }
        Item::Fn { body, .. } => {
            if let Some(stmts) = body {
                for s in stmts {
                    collect_stmt(s, out);
                }
            }
        }
        Item::Test { body, .. } | Item::Comptime { body, .. } => {
            for s in body {
                collect_stmt(s, out);
            }
        }
    }
}

/// Collects imports from one statement (only the import-bearing forms matter:
/// `const`/`var` initializers and bare expressions).
fn collect_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::Const { value, .. } => collect_expr(value, out),
        Stmt::Var { value: Some(v), .. } => collect_expr(v, out),
        Stmt::Expr { expr, .. } => collect_expr(expr, out),
        _ => {}
    }
}

/// Collects an `@import("...")` literal if `e` is one. (Imports are always a
/// direct `@import` builtin with a single string literal; we do not recurse into
/// arbitrary sub-expressions, which never contain a binding-introducing import.)
fn collect_expr(e: &Expr, out: &mut Vec<String>) {
    if let Some(s) = import_literal(e) {
        out.push(s);
    }
}

/// If `e` is exactly `@import("name")`, returns the imported `name` (quotes
/// stripped). Returns `None` for any other expression.
pub fn import_literal(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                return Some(strip_quotes(text));
            }
        }
    }
    None
}

/// Strips a leading/trailing `"` from a string-literal's raw text.
fn strip_quotes(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

/// Rewrites every `@import("X")` occurrence in `source` to the bare identifier
/// `replace(X)` returns (or leaves it untouched when `replace` returns `None`).
///
/// This is a lexical scan that finds `@import` followed (across whitespace) by
/// `(` `"…"` `)`, and replaces the WHOLE `@import("…")` span with the bare
/// identifier. String contents are not parsed for nested quotes — a k2 import
/// string is a simple double-quoted literal with no escaped quotes (a path or a
/// bare name), so a flat scan to the closing `"` is exact. Everything outside the
/// matched spans (comments, code, formatting) is preserved byte-for-byte.
pub fn rewrite_import_strings<F>(source: &str, mut replace: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    // `copied_to` is the start of the not-yet-emitted run of original bytes. When
    // a match fires we flush `[copied_to..match_start)` as a `&str` slice (always
    // a valid UTF-8 boundary, since a match begins only at an ASCII `@`), emit the
    // replacement, and resume after the match — so multi-byte UTF-8 (em-dashes,
    // etc.) in comments/strings is copied verbatim, never byte-mangled.
    let mut copied_to = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            if let Some((end, raw)) = match_import_at(bytes, i) {
                if let Some(repl) = replace(&raw) {
                    out.push_str(&source[copied_to..i]);
                    out.push_str(&repl);
                    copied_to = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&source[copied_to..]);
    out
}

/// Tries to match `@import ( "…" )` starting at byte `i`. On success returns
/// `(end_index_after_close_paren, import_string)`. Whitespace is allowed between
/// the tokens (but not inside the string).
fn match_import_at(bytes: &[u8], i: usize) -> Option<(usize, String)> {
    const KW: &[u8] = b"@import";
    if i + KW.len() > bytes.len() || &bytes[i..i + KW.len()] != KW {
        return None;
    }
    // The char before `@import` (if any) must not be an identifier char, so we do
    // not match `foo@import` mid-token. `@` cannot follow an ident char in k2, so
    // this is belt-and-suspenders.
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return None;
    }
    let mut j = i + KW.len();
    j = skip_ws(bytes, j);
    if j >= bytes.len() || bytes[j] != b'(' {
        return None;
    }
    j = skip_ws(bytes, j + 1);
    if j >= bytes.len() || bytes[j] != b'"' {
        return None;
    }
    let str_start = j + 1;
    let mut k = str_start;
    while k < bytes.len() && bytes[k] != b'"' {
        k += 1;
    }
    if k >= bytes.len() {
        return None;
    }
    let raw = String::from_utf8_lossy(&bytes[str_start..k]).into_owned();
    let mut m = skip_ws(bytes, k + 1);
    if m >= bytes.len() || bytes[m] != b')' {
        return None;
    }
    m += 1;
    Some((m, raw))
}

/// Skips ASCII whitespace from `i`, returning the first non-whitespace index.
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `true` if `b` is an identifier byte (`[A-Za-z0-9_]`).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_basic_import() {
        let src = "const x = @import(\"./a.k2\");\n";
        let out = rewrite_import_strings(src, |raw| {
            assert_eq!(raw, "./a.k2");
            Some("__k2_mod_abc".to_string())
        });
        assert_eq!(out, "const x = __k2_mod_abc;\n");
    }

    #[test]
    fn leaves_unmapped_opaque() {
        let src = "const j = @import(\"json\");\n";
        let out = rewrite_import_strings(src, |_| None);
        assert_eq!(out, src);
    }

    #[test]
    fn preserves_comments_and_code() {
        let src = "// hi\nconst s = @import(\"std\"); // tail\nfn f() void {}\n";
        let out = rewrite_import_strings(src, |raw| {
            if raw == "std" {
                Some("__k2_std_root".to_string())
            } else {
                None
            }
        });
        assert_eq!(
            out,
            "// hi\nconst s = __k2_std_root; // tail\nfn f() void {}\n"
        );
    }

    #[test]
    fn handles_whitespace_in_call() {
        let src = "const x = @import ( \"a/b.k2\" )  ;\n";
        let out = rewrite_import_strings(src, |_| Some("M".to_string()));
        assert_eq!(out, "const x = M  ;\n");
    }
}
