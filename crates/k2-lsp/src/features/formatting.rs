//! `textDocument/formatting`: run the canonical formatter and return a single
//! full-document `TextEdit`.
//!
//! This delegates to `k2_fmt::format_source` — the *same* engine `k2c fmt` uses —
//! so format-on-save and the CLI produce byte-identical output by construction.
//! `format_source` refuses to format input with any error-severity parse
//! diagnostic; in that case we return `null` (there is nothing safe to format).

use crate::analysis::Analysis;
use crate::json::JsonValue;

/// Computes the formatting edits for an analysis: either a one-element array with
/// a whole-document `TextEdit`, or `null` when the source does not parse.
pub fn compute(analysis: &Analysis) -> JsonValue {
    let formatted = match k2_fmt::format_source(&analysis.source) {
        Ok(text) => text,
        Err(_) => return JsonValue::Null,
    };

    // A single edit replacing the entire buffer (start of document to its end).
    let range = JsonValue::obj(vec![
        (
            "start",
            JsonValue::obj(vec![
                ("line", JsonValue::num(0)),
                ("character", JsonValue::num(0)),
            ]),
        ),
        ("end", analysis.posmap.end_position_json()),
    ]);
    let edit = JsonValue::obj(vec![
        ("range", range),
        ("newText", JsonValue::str(formatted)),
    ]);
    JsonValue::arr(vec![edit])
}
