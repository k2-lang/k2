//! `textDocument/codeAction`: quick-fixes derived from diagnostics.
//!
//! The provider re-derives the document's diagnostics from the cached analysis
//! (rather than trusting the client's echo), and for each diagnostic that carries
//! a backtick-quoted *suggestion* in its `help` text it offers an "apply
//! suggestion" quick-fix. The canonical case is the resolver's undeclared-name
//! help, `` "a binding named `X` exists — did you mean it?" ``: the action
//! replaces the offending identifier token (the diagnostic's own span) with `X`.
//!
//! Only diagnostics whose span intersects the requested `range` are offered. Each
//! `CodeAction` carries an inline `WorkspaceEdit`, so the client applies it
//! without a follow-up resolve round-trip. The extraction is generic: any future
//! diagnostic whose help quotes a single replacement identifier in backticks opts
//! in automatically.

use crate::analysis::Analysis;
use crate::json::JsonValue;

/// Computes the `CodeAction[]` for the requested `[lo, hi)` scalar range.
pub fn compute(analysis: &Analysis, uri: &str, lo: u32, hi: u32) -> JsonValue {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::arr(Vec::new()),
    };

    let mut actions: Vec<JsonValue> = Vec::new();
    for d in &resolved.diagnostics {
        // Only fixes for diagnostics overlapping the requested range.
        if d.span.end < lo || d.span.start > hi {
            continue;
        }
        let help = match &d.help {
            Some(h) => h,
            None => continue,
        };
        let suggestion = match extract_suggestion(help) {
            Some(s) => s,
            None => continue,
        };
        // Skip a non-identifier suggestion defensively (cannot produce a valid edit).
        if !crate::features::is_valid_ident(&suggestion) {
            continue;
        }
        actions.push(quick_fix(analysis, uri, d.span, &suggestion));
    }
    JsonValue::arr(actions)
}

/// Extracts the first backtick-quoted token from a help string, e.g. the `X` in
/// `` "a binding named `X` exists — did you mean it?" ``.
fn extract_suggestion(help: &str) -> Option<String> {
    let open = help.find('`')? + 1;
    let rest = &help[open..];
    let close = rest.find('`')?;
    Some(rest[..close].to_string())
}

/// Builds an "apply suggestion" `CodeAction` whose edit replaces the diagnostic's
/// span with the suggested identifier.
fn quick_fix(analysis: &Analysis, uri: &str, span: k2_syntax::Span, suggestion: &str) -> JsonValue {
    let edit = JsonValue::obj(vec![
        ("range", analysis.posmap.span_to_range(span)),
        ("newText", JsonValue::str(suggestion)),
    ]);
    let changes = JsonValue::Object(vec![(uri.to_string(), JsonValue::arr(vec![edit]))]);
    let workspace_edit = JsonValue::obj(vec![("changes", changes)]);
    // The matching diagnostic is echoed so the client can dismiss it on apply.
    let diagnostic = JsonValue::obj(vec![
        ("range", analysis.posmap.span_to_range(span)),
        ("severity", JsonValue::num(1)),
        ("source", JsonValue::str("k2c")),
    ]);
    JsonValue::obj(vec![
        ("title", JsonValue::str(format!("Change to `{suggestion}`"))),
        ("kind", JsonValue::str("quickfix")),
        ("diagnostics", JsonValue::arr(vec![diagnostic])),
        ("edit", workspace_edit),
    ])
}
