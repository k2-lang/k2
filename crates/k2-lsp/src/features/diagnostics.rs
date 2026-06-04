//! `textDocument/publishDiagnostics`: collect diagnostics from every front-end
//! stage that ran and map them to LSP `Diagnostic`s with exact ranges.
//!
//! Diagnostics come from the parser, the resolver, and the type checker. Each
//! stage's `Diagnostic` is a distinct (but identically shaped) type, so this
//! module funnels them through a small common form before mapping. Ranges are
//! always recomputed from the scalar `span.start`/`span.end` via the position
//! map — never from `span.line`/`col`, which is a scalar column count, not the
//! UTF-16 LSP expects.

use crate::analysis::Analysis;
use crate::json::JsonValue;
use crate::position::PositionMap;

/// One stage's diagnostic, normalized to a span + severity + message.
struct Diag {
    start: u32,
    end: u32,
    /// LSP severity: 1 = Error, 2 = Warning.
    severity: i64,
    message: String,
}

/// Computes the LSP `diagnostics` array for an analysis.
pub fn compute(analysis: &Analysis) -> JsonValue {
    let mut diags: Vec<Diag> = Vec::new();

    // Parser diagnostics always apply.
    for d in &analysis.parse.diagnostics {
        diags.push(Diag {
            start: d.span.start,
            end: d.span.end,
            severity: match d.severity {
                k2_parse::Severity::Error => 1,
                k2_parse::Severity::Warning => 2,
            },
            message: d.message.clone(),
        });
    }

    // Resolver diagnostics, if resolution ran.
    if let Some(resolved) = &analysis.resolved {
        for d in &resolved.diagnostics {
            diags.push(Diag {
                start: d.span.start,
                end: d.span.end,
                severity: if d.is_error() { 1 } else { 2 },
                message: d.message.clone(),
            });
        }
    }

    // Type diagnostics, if checking ran.
    if let Some(typed) = &analysis.typed {
        for d in &typed.diagnostics {
            diags.push(Diag {
                start: d.span.start,
                end: d.span.end,
                severity: if d.is_error() { 1 } else { 2 },
                message: d.message.clone(),
            });
        }
    }

    let items: Vec<JsonValue> = diags
        .into_iter()
        .map(|d| to_lsp(&analysis.posmap, d))
        .collect();
    JsonValue::arr(items)
}

/// Maps one normalized diagnostic to an LSP `Diagnostic` JSON object.
fn to_lsp(posmap: &PositionMap, d: Diag) -> JsonValue {
    let range = JsonValue::obj(vec![
        ("start", posmap.offset_to_position_json(d.start)),
        ("end", posmap.offset_to_position_json(d.end)),
    ]);
    JsonValue::obj(vec![
        ("range", range),
        ("severity", JsonValue::num(d.severity)),
        ("source", JsonValue::str("k2c")),
        ("message", JsonValue::str(d.message)),
    ])
}
