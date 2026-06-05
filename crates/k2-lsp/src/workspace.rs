//! The cross-file workspace layer: a lightweight project module graph over the
//! open documents, used to resolve **go-to-definition and references across a
//! path import**.
//!
//! ## What is cross-file, and what is not
//!
//! The compiler's multi-file merge (`k2c::multi`) is a *textual* concatenation
//! that re-keys every span into one merged buffer. Re-keying merged spans back to
//! each file's own UTF-16 positions for every feature would be large, so — per the
//! milestone's documented boundary — only **definition** and **references** cross
//! a path import here. signatureHelp, inlayHint, semanticTokens, codeAction, and
//! rename stay within the single open buffer. Named-module imports (`std`) remain
//! opaque, exactly as the within-file `definition` already treats them.
//!
//! The model is deliberately additive: it reads the already-open document buffers
//! (and falls back to reading a `.k2` file from disk when the import target is not
//! open), parses+resolves each on demand, and never mutates the document store.
//!
//! A cross-file query works like this:
//!
//! * **Definition** on `b.foo`, where `b = @import("./b.k2")`: resolve the import
//!   string relative to the importing file's URI to a target URI, parse+resolve
//!   that file, find the top-level item named `foo`, and return a `Location` in
//!   the *target* file's own coordinates.
//! * **References** of a top-level item `foo` defined in the current file: scan
//!   every other open document for a member access `b.foo` where `b` imports the
//!   current file, and add those occurrences (in their own file's coordinates).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use k2_resolve::{classify_import, resolve_path_lenient, ImportSpec};
use k2_syntax::{Expr, Item, SourceFile};

use crate::analysis::Analysis;
use crate::document::DocumentStore;
use crate::json::JsonValue;

/// Decodes a `file://` URI to a filesystem path. Returns `None` for a non-file
/// URI (so an in-memory/untitled document simply has no cross-file graph).
///
/// Only the common `file:///abs/path` form is handled (with minimal `%XX`
/// percent-decoding); a host component or a non-`file` scheme yields `None`.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an optional empty authority: `file:///x` → `/x`.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(path);
    Some(PathBuf::from(decoded))
}

/// Encodes a filesystem path back into a `file://` URI (the inverse of
/// [`uri_to_path`] for the paths we produce). Only `/` separators and the bytes we
/// percent-decode are handled, which covers the editor URIs we round-trip.
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("file://{}", percent_encode(&s))
}

/// Minimal `%XX` percent-decoding (sufficient for spaces and the few reserved
/// characters an editor escapes in a file URI).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes the characters an editor would escape in a `file://` path
/// (only a space here; ASCII path bytes otherwise pass through unchanged).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == ' ' {
            out.push_str("%20");
        } else {
            out.push(c);
        }
    }
    out
}

/// The numeric value of a single hex digit byte.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Loads the source text of a target file by URI: from an open document if one
/// exists, otherwise from disk. Returns `None` if neither is available.
fn load_source(store: &DocumentStore, target_uri: &str) -> Option<String> {
    if let Some(doc) = store.get(target_uri) {
        return Some(doc.text.clone());
    }
    let path = uri_to_path(target_uri)?;
    std::fs::read_to_string(path).ok()
}

/// Resolves a relative path import in `importer_uri` to the imported file's URI,
/// or `None` if `importer_uri` is not a `file://` URI.
fn import_target_uri(importer_uri: &str, rel: &str) -> Option<String> {
    let importer = uri_to_path(importer_uri)?;
    let dir = importer.parent().unwrap_or_else(|| Path::new("."));
    let target = resolve_path_lenient(dir, rel);
    Some(path_to_uri(&target))
}

/// The set of `(local module name → imported file URI)` bindings in `file`: every
/// top-level `const name = @import("./path.k2")` whose argument is a path import.
fn import_bindings(importer_uri: &str, file: &SourceFile) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for item in &file.items {
        if let Item::Const { name, value, .. } = item {
            if let Some(rel) = path_import_literal(value) {
                if let Some(target) = import_target_uri(importer_uri, &rel) {
                    out.insert(name.clone(), target);
                }
            }
        }
    }
    out
}

/// If `e` is `@import("X")` where `X` is a *path* import, returns `X`.
fn path_import_literal(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                let raw = strip_quotes(text);
                if let ImportSpec::Path(p) = classify_import(&raw) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Strips surrounding double quotes from a string literal's raw lexeme.
fn strip_quotes(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

/// Attempts a **cross-file definition** for a member access at scalar `offset` in
/// the document `uri`. Returns the target `Location` if the access is `b.member`
/// where `b` is a path import and the target file declares a top-level `member`.
/// Returns `None` to let the within-file `definition` provider answer instead.
pub fn cross_file_definition(
    store: &DocumentStore,
    analysis: &Analysis,
    uri: &str,
    offset: u32,
) -> Option<JsonValue> {
    let (module_name, member, _span) = member_access_at(analysis, offset)?;
    let bindings = import_bindings(uri, &analysis.parse.file);
    let target_uri = bindings.get(&module_name)?.clone();

    // Parse + resolve the target file in its own coordinates.
    let target_src = load_source(store, &target_uri)?;
    let target = Analysis::compute(target_src);
    let item_span = top_level_item_span(&target.parse.file, &member)?;
    Some(JsonValue::obj(vec![
        ("uri", JsonValue::str(target_uri)),
        ("range", target.posmap.span_to_range(item_span)),
    ]))
}

/// Collects **cross-file references** to a top-level item named `item_name`
/// defined in `def_uri`: for every *other* open document that path-imports
/// `def_uri`, every member access `b.item_name` (where `b` is that import's local
/// name) becomes a `Location` in the using file's coordinates.
pub fn cross_file_references(
    store: &DocumentStore,
    def_uri: &str,
    item_name: &str,
) -> Vec<JsonValue> {
    let def_path = match uri_to_path(def_uri) {
        Some(p) => normalize(&p),
        None => return Vec::new(),
    };
    let mut out: Vec<JsonValue> = Vec::new();
    for (other_uri, src) in store.iter_texts() {
        if other_uri == def_uri {
            continue;
        }
        let analysis = Analysis::compute(src);
        let bindings = import_bindings(&other_uri, &analysis.parse.file);
        // The local names that import the defining file.
        let names: Vec<&String> = bindings
            .iter()
            .filter(|(_, target)| {
                uri_to_path(target).map(|p| normalize(&p)) == Some(def_path.clone())
            })
            .map(|(name, _)| name)
            .collect();
        if names.is_empty() {
            continue;
        }
        for (mod_name, member, span) in all_member_accesses(&analysis) {
            if member == item_name && names.iter().any(|n| **n == mod_name) {
                out.push(JsonValue::obj(vec![
                    ("uri", JsonValue::str(other_uri.clone())),
                    ("range", analysis.posmap.span_to_range(span)),
                ]));
            }
        }
    }
    out
}

/// Lexically normalizes a path (collapsing `.`/`..`) so two URIs to the same file
/// compare equal regardless of how the import string spelled the path.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for comp in out {
        buf.push(comp.as_os_str());
    }
    buf
}

/// If `offset` rests on the member token of a `module.member` field access whose
/// base is a bare identifier, returns `(module_name, member_name, member_span)`.
///
/// Only a single-level `b.foo` access with an identifier base is recognized — the
/// common imported-namespace member form. The returned span covers just the
/// member token, for highlighting the reference.
fn member_access_at(analysis: &Analysis, offset: u32) -> Option<(String, String, k2_syntax::Span)> {
    let mut found: Option<(String, String, k2_syntax::Span)> = None;
    for item in &analysis.parse.file.items {
        walk_item_fields(item, offset, &mut found);
    }
    found
}

/// Every `module.member` access in the file, as `(module, member, member_span)`,
/// for the references scan.
fn all_member_accesses(analysis: &Analysis) -> Vec<(String, String, k2_syntax::Span)> {
    let mut out = Vec::new();
    for item in &analysis.parse.file.items {
        collect_item_fields(item, &mut out);
    }
    out
}

/// Recurses an item looking for the narrowest `ident.member` access covering
/// `offset`.
fn walk_item_fields(
    item: &Item,
    offset: u32,
    found: &mut Option<(String, String, k2_syntax::Span)>,
) {
    each_expr_in_item(item, &mut |e| walk_expr_fields(e, offset, found));
}

/// Recurses an item collecting every `ident.member` access.
fn collect_item_fields(item: &Item, out: &mut Vec<(String, String, k2_syntax::Span)>) {
    each_expr_in_item(item, &mut |e| collect_expr_fields(e, out));
}

/// Finds the member token span of an `ident.field` access covering `offset`.
fn walk_expr_fields(e: &Expr, offset: u32, found: &mut Option<(String, String, k2_syntax::Span)>) {
    if let Expr::Field { base, field, span } = e {
        if let Expr::Ident { name, .. } = base.as_ref() {
            // The member token occupies [base.end + dot, span.end). Approximate the
            // member start as span.end - field.chars().count() so a cursor on the
            // member (not the base) is required.
            let member_len = field.chars().count() as u32;
            let member_start = span.end.saturating_sub(member_len);
            if member_start <= offset && offset <= span.end {
                let member_span = k2_syntax::Span::new(member_start, span.end, span.line, span.col);
                *found = Some((name.clone(), field.clone(), member_span));
            }
        }
    }
    for child in field_children(e) {
        walk_expr_fields(child, offset, found);
    }
}

/// Collects every `ident.field` access in `e`.
fn collect_expr_fields(e: &Expr, out: &mut Vec<(String, String, k2_syntax::Span)>) {
    if let Expr::Field { base, field, span } = e {
        if let Expr::Ident { name, .. } = base.as_ref() {
            let member_len = field.chars().count() as u32;
            let member_start = span.end.saturating_sub(member_len);
            let member_span = k2_syntax::Span::new(member_start, span.end, span.line, span.col);
            out.push((name.clone(), field.clone(), member_span));
        }
    }
    for child in field_children(e) {
        collect_expr_fields(child, out);
    }
}

/// Invokes `f` on every top-level expression of an item (declaration initializers
/// and function/test/comptime body statement expressions).
fn each_expr_in_item<'a>(item: &'a Item, f: &mut impl FnMut(&'a Expr)) {
    use k2_syntax::Stmt;
    fn each_stmt<'a>(stmts: &'a [Stmt], f: &mut impl FnMut(&'a Expr)) {
        for s in stmts {
            match s {
                Stmt::Const { value, .. } => f(value),
                Stmt::Var { value: Some(v), .. } => f(v),
                Stmt::Return { value: Some(v), .. } => f(v),
                Stmt::Expr { expr, .. } => f(expr),
                Stmt::Assign { target, value, .. } => {
                    f(target);
                    f(value);
                }
                Stmt::Comptime { body, .. } | Stmt::Block { body, .. } => each_stmt(body, f),
                Stmt::If { expr, .. }
                | Stmt::While { expr, .. }
                | Stmt::For { expr, .. }
                | Stmt::Switch { expr, .. } => f(expr),
                _ => {}
            }
        }
    }
    match item {
        Item::Const { value, .. } => f(value),
        Item::Var { value: Some(v), .. } => f(v),
        Item::Fn {
            body: Some(body), ..
        } => each_stmt(body, f),
        Item::Test { body, .. } | Item::Comptime { body, .. } => each_stmt(body, f),
        _ => {}
    }
}

/// The direct sub-expressions of `e` for the field-access walk.
fn field_children(e: &Expr) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    match e {
        Expr::Builtin { args, .. } => out.extend(args.iter()),
        Expr::Field { base, .. } => out.push(base),
        Expr::Call { callee, args, .. } => {
            out.push(callee);
            out.extend(args.iter());
        }
        Expr::Binary { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        Expr::Unary { operand, .. } => out.push(operand),
        Expr::Index { base, index, .. } => {
            out.push(base);
            out.push(index);
        }
        Expr::Deref { base, .. } | Expr::Unwrap { base, .. } => out.push(base),
        Expr::Catch { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        _ => {}
    }
    out
}

/// The defining span of a top-level item named `name` in `file` (a `const`/`var`/
/// `fn` declaration), used as a cross-file definition target.
fn top_level_item_span(file: &SourceFile, name: &str) -> Option<k2_syntax::Span> {
    for item in &file.items {
        let item_name = match item {
            Item::Const { name, .. } | Item::Var { name, .. } | Item::Fn { name, .. } => {
                Some(name.as_str())
            }
            _ => None,
        };
        if item_name == Some(name) {
            // Point at the item's whole span; the within-file definition uses the
            // binding-name `Def` span, but cross-file we lack that table, so the
            // item span is the stable, exact-enough target.
            return Some(item.span());
        }
    }
    None
}
