//! Crate-level tests: the feature providers against known sources, plus a
//! scripted end-to-end JSON-RPC session driving the server over in-memory
//! streams (the same code path as `k2c lsp` over stdio, minus the OS pipe).

use std::io::Cursor;

use crate::analysis::Analysis;
use crate::json::{parse_json, to_json_string, JsonValue};
use crate::server::Server;

/// Builds an analysis from a literal source.
fn analyze(src: &str) -> Analysis {
    Analysis::compute(src.to_string())
}

/// The scalar (char) offset of the start of the first occurrence of `needle` in
/// `src`, matching compiler spans.
fn char_offset(src: &str, needle: &str) -> u32 {
    let byte = src.find(needle).expect("needle present");
    src[..byte].chars().count() as u32
}

/// The completion labels in a completion result.
fn labels(result: &JsonValue) -> Vec<String> {
    result
        .get("items")
        .and_then(|i| i.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.get("label").and_then(|l| l.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// =========================================================================
//  Diagnostics
// =========================================================================

#[test]
fn diagnostics_clean_source_is_empty() {
    let a = analyze("const x: i32 = 1;\n");
    let diags = crate::features::diagnostics::compute(&a);
    assert_eq!(diags.as_array().map(|a| a.len()), Some(0));
}

#[test]
fn diagnostics_report_undeclared_identifier() {
    // `y` is never declared — a resolve error.
    let src = "pub fn main() void {\n    const z = y;\n}\n";
    let a = analyze(src);
    let diags = crate::features::diagnostics::compute(&a);
    let items = diags.as_array().unwrap();
    assert!(!items.is_empty(), "expected at least one diagnostic");
    let d = &items[0];
    assert_eq!(d.get("severity").and_then(|s| s.as_i64()), Some(1));
    assert_eq!(d.get("source").and_then(|s| s.as_str()), Some("k2c"));
    let msg = d.get("message").and_then(|m| m.as_str()).unwrap();
    assert!(msg.contains('y'), "message mentions the bad name: {msg}");
    // The range points at line 1 (0-based), where `y` is.
    let line = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|l| l.as_i64());
    assert_eq!(line, Some(1));
}

#[test]
fn diagnostics_report_type_error_with_range() {
    // Assigning a bool to an i32 const is a type error.
    let src = "const x: i32 = true;\n";
    let a = analyze(src);
    let diags = crate::features::diagnostics::compute(&a);
    let items = diags.as_array().unwrap();
    assert!(!items.is_empty(), "expected a type diagnostic");
    assert!(items
        .iter()
        .all(|d| d.get("severity").and_then(|s| s.as_i64()) == Some(1)));
}

// =========================================================================
//  Hover
// =========================================================================

#[test]
fn hover_returns_type_of_local() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    // hover on the `x` use in `const y = x;`
    let off = char_offset(src, "= x;") + 2;
    let hover = crate::features::hover::compute(&a, off);
    let value = hover
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(value.contains("x: i32"), "hover value: {value}");
}

#[test]
fn hover_on_whitespace_is_null() {
    let src = "const x: i32 = 1;\n";
    let a = analyze(src);
    // Offset 0 is on the `const` keyword, not an identifier occurrence.
    let hover = crate::features::hover::compute(&a, 0);
    assert_eq!(hover, JsonValue::Null);
}

#[test]
fn hover_on_member_shows_member_name_and_tight_range() {
    // Hover on a struct-field access must show the field name before the colon
    // (recovered from the source, since the resolver records member uses with an
    // empty name) and highlight only the member identifier, not `p.xx`.
    let src = "const Point = struct {\n    xx: i32,\n};\npub fn main() void {\n    const p: Point = .{ .xx = 1 };\n    const a = p.xx;\n}\n";
    let a = analyze(src);
    // cursor on the `xx` member of `p.xx`.
    let off = char_offset(src, "p.xx;") + 2;
    let hover = crate::features::hover::compute(&a, off);
    let value = hover
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(
        value.contains("xx: i32"),
        "member hover must name the field: {value}"
    );
    assert!(
        !value.contains("\n: "),
        "member hover must not have an empty name: {value}"
    );
    // The range covers just `xx`, not the whole `p.xx` access. On that line the
    // access `p.xx` starts at column 14, so the member `xx` is at column 16.
    let range = hover.get("range").unwrap();
    let start = range.get("start").unwrap();
    let end = range.get("end").unwrap();
    assert_eq!(
        start.get("character").and_then(|c| c.as_i64()),
        Some(16),
        "range starts at the member `xx`, not the base `p.`"
    );
    assert_eq!(
        end.get("character").and_then(|c| c.as_i64()),
        Some(18),
        "range ends at the end of `xx`"
    );
    // Single line, so start and end share a line.
    assert_eq!(
        start.get("line").and_then(|l| l.as_i64()),
        end.get("line").and_then(|l| l.as_i64())
    );
}

#[test]
fn hover_at_end_of_identifier_resolves() {
    // A caret resting just past the last char of an identifier must still hover
    // (inclusive upper bound in `use_at_offset`).
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    // offset just past the `x` use in `const y = x;`.
    let off = char_offset(src, "= x;") + 3;
    let hover = crate::features::hover::compute(&a, off);
    let value = hover
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        value.contains("x: i32"),
        "cursor at end of `x` should hover: {value}"
    );
}

#[test]
fn definition_at_end_of_identifier_resolves() {
    // The same inclusive-end behavior for go-to-definition.
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= x;") + 3; // just past `x`
    let def = crate::features::definition::compute(&a, "u", off);
    let start = def.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    assert_eq!(start.get("character").and_then(|c| c.as_i64()), Some(0));
}

#[test]
fn hover_on_function_shows_fn_type() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "add(1, 2)");
    let hover = crate::features::hover::compute(&a, off);
    let value = hover
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        value.contains("fn("),
        "expected a fn type in hover: {value}"
    );
}

// =========================================================================
//  Definition
// =========================================================================

#[test]
fn definition_jumps_to_const_declaration() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= x;") + 2;
    let def = crate::features::definition::compute(&a, "file:///t.k2", off);
    assert_eq!(
        def.get("uri").and_then(|u| u.as_str()),
        Some("file:///t.k2")
    );
    // `const x` is on line 0; the def span starts at char 0.
    let start = def.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    assert_eq!(start.get("character").and_then(|c| c.as_i64()), Some(0));
}

#[test]
fn definition_on_predeclared_is_null() {
    let src = "const x: i32 = 1;\n";
    let a = analyze(src);
    // def on the `i32` type annotation.
    let off = char_offset(src, "i32");
    let def = crate::features::definition::compute(&a, "u", off);
    assert_eq!(def, JsonValue::Null);
}

#[test]
fn definition_on_param_use() {
    let src = "pub fn f(value: i32) i32 {\n    return value;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "return value") + 7;
    let def = crate::features::definition::compute(&a, "u", off);
    // The parameter `value` is declared at its first occurrence on line 0.
    let start = def.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
}

#[test]
fn definition_on_field_access() {
    let src = "const Point = struct {\n    x: i32,\n    y: i32,\n};\npub fn main() void {\n    const p: Point = .{ .x = 1, .y = 2 };\n    const a = p.x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "p.x;") + 2;
    let def = crate::features::definition::compute(&a, "u", off);
    // The field `x` is declared on line 1.
    let start = def.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(1));
}

// =========================================================================
//  Completion
// =========================================================================

#[test]
fn completion_scope_aware_includes_locals_params_items_and_predeclared() {
    let src = "const top: i32 = 0;\npub fn main(sys: *System) void {\n    const local = 1;\n    const z = local;\n}\n";
    let a = analyze(src);
    // bare-prefix completion at the `local` use in `const z = local;`.
    let off = char_offset(src, "= local;") + 2;
    let result = crate::features::completion::compute(&a, off);
    let got = labels(&result);
    for expected in ["top", "main", "sys", "local", "i32", "bool", "System"] {
        assert!(
            got.contains(&expected.to_string()),
            "missing {expected}: {got:?}"
        );
    }
}

#[test]
fn completion_excludes_struct_fields_from_bare_scope() {
    let src = "const Point = struct {\n    fieldname: i32,\n};\npub fn main() void {\n    const q = Point;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= Point;") + 2;
    let result = crate::features::completion::compute(&a, off);
    let got = labels(&result);
    // `Point` (a file item) is offered; its field `fieldname` is NOT bare-visible.
    assert!(got.contains(&"Point".to_string()));
    assert!(
        !got.contains(&"fieldname".to_string()),
        "fields must not appear bare: {got:?}"
    );
}

#[test]
fn completion_respects_local_declaration_order() {
    // A local declared *after* the cursor is not yet in scope (k2 locals are
    // order-dependent), so completion must not offer it. `after` is declared
    // below the cursor and must be excluded; `before` is declared above and is
    // offered.
    let src =
        "pub fn main() void {\n    const before = 1;\n    const z = aft;\n    const after = 2;\n}\n";
    let a = analyze(src);
    // cursor right after the `aft` prefix in `const z = aft;`.
    let off = char_offset(src, "aft;") + 3;
    let result = crate::features::completion::compute(&a, off);
    let got = labels(&result);
    assert!(
        !got.contains(&"after".to_string()),
        "must not offer a not-yet-declared local: {got:?}"
    );

    // With an empty prefix at the same point, `before` is visible but `after`
    // (declared later) still is not.
    let off2 = char_offset(src, "= aft;") + 2;
    let result2 = crate::features::completion::compute(&a, off2);
    let got2 = labels(&result2);
    assert!(
        got2.contains(&"before".to_string()),
        "an earlier local must be offered: {got2:?}"
    );
    assert!(
        !got2.contains(&"after".to_string()),
        "a later local must not be offered: {got2:?}"
    );
}

#[test]
fn completion_member_after_dot_lists_struct_members() {
    let src = "const Point = struct {\n    x: i32,\n    y: i32,\n    pub fn mag(self: *Point) i32 { return self.x; }\n};\npub fn main() void {\n    const p: Point = .{ .x = 1, .y = 2 };\n    const a = p.x;\n}\n";
    let a = analyze(src);
    // cursor right after the `.` in `p.x` (member completion).
    let off = char_offset(src, "p.x;") + 2;
    let result = crate::features::completion::compute(&a, off);
    let got = labels(&result);
    assert!(got.contains(&"x".to_string()), "fields: {got:?}");
    assert!(got.contains(&"y".to_string()), "fields: {got:?}");
    assert!(got.contains(&"mag".to_string()), "method: {got:?}");
    // No bare scope leakage: predeclared names must not appear after `.`.
    assert!(
        !got.contains(&"i32".to_string()),
        "no scope names after dot: {got:?}"
    );
}

// =========================================================================
//  Formatting
// =========================================================================

#[test]
fn formatting_returns_single_full_document_edit() {
    let src = "const  x=1 ;\n";
    let a = analyze(src);
    let result = crate::features::formatting::compute(&a);
    let edits = result.as_array().unwrap();
    assert_eq!(edits.len(), 1);
    let edit = &edits[0];
    assert_eq!(
        edit.get("newText").and_then(|t| t.as_str()),
        Some("const x = 1;\n")
    );
    // The range starts at the document origin.
    let start = edit.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    assert_eq!(start.get("character").and_then(|c| c.as_i64()), Some(0));
}

#[test]
fn formatting_returns_null_on_parse_error() {
    let src = "const x = ;\n"; // missing expression
    let a = analyze(src);
    let result = crate::features::formatting::compute(&a);
    assert_eq!(result, JsonValue::Null);
}

// =========================================================================
//  Scripted end-to-end JSON-RPC session
// =========================================================================

/// Frames a JSON value as a `Content-Length` message.
fn frame(value: &JsonValue) -> String {
    let body = to_json_string(value);
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

/// Splits a stream of `Content-Length`-framed messages into the parsed JSON
/// bodies (the inverse of [`frame`]).
fn unframe(mut bytes: &[u8]) -> Vec<JsonValue> {
    let mut out = Vec::new();
    while let Some(header_end) = find_subslice(bytes, b"\r\n\r\n") {
        let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let len: usize = header
            .lines()
            .find_map(|l| {
                l.split_once(':').and_then(|(k, v)| {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
            })
            .expect("Content-Length header");
        let body_start = header_end + 4;
        let body = &bytes[body_start..body_start + len];
        out.push(parse_json(std::str::from_utf8(body).unwrap()).unwrap());
        bytes = &bytes[body_start + len..];
    }
    out
}

/// Finds the first index of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Position JSON object helper.
fn pos(line: i64, character: i64) -> JsonValue {
    JsonValue::obj(vec![
        ("line", JsonValue::num(line)),
        ("character", JsonValue::num(character)),
    ])
}

/// Builds a request envelope.
fn request(id: i64, method: &str, params: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("id", JsonValue::num(id)),
        ("method", JsonValue::str(method)),
        ("params", params),
    ])
}

/// Builds a notification envelope.
fn notif(method: &str, params: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("method", JsonValue::str(method)),
        ("params", params),
    ])
}

/// A `textDocument`-only params object (uri reference).
fn td(uri: &str) -> JsonValue {
    JsonValue::obj(vec![(
        "textDocument",
        JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
    )])
}

/// A position request params object.
fn td_pos(uri: &str, position: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        (
            "textDocument",
            JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
        ),
        ("position", position),
    ])
}

/// Runs the server over a list of framed messages, returning the parsed outputs.
fn run_session(messages: &[JsonValue]) -> Vec<JsonValue> {
    let mut input = String::new();
    for m in messages {
        input.push_str(&frame(m));
    }
    let mut reader = Cursor::new(input.into_bytes());
    let mut output: Vec<u8> = Vec::new();
    let mut server = Server::new();
    let _ = server.serve(&mut reader, &mut output);
    unframe(&output)
}

/// Finds a response by its echoed id.
fn by_id(messages: &[JsonValue], id: i64) -> &JsonValue {
    messages
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_i64()) == Some(id))
        .unwrap_or_else(|| panic!("no message with id {id}"))
}

#[test]
fn scripted_session_drives_every_feature() {
    let uri = "file:///session.k2";
    let doc = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";

    let did_open = notif(
        "textDocument/didOpen",
        JsonValue::obj(vec![(
            "textDocument",
            JsonValue::obj(vec![
                ("uri", JsonValue::str(uri)),
                ("languageId", JsonValue::str("k2")),
                ("version", JsonValue::num(1)),
                ("text", JsonValue::str(doc)),
            ]),
        )]),
    );

    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        did_open,
        // hover/definition on the `x` use (line 2, char 14).
        request(2, "textDocument/hover", td_pos(uri, pos(2, 14))),
        request(3, "textDocument/definition", td_pos(uri, pos(2, 14))),
        // completion inside the body at the start of `x` (empty prefix → all
        // visible names, so both `x` and `main` appear).
        request(4, "textDocument/completion", td_pos(uri, pos(2, 14))),
        request(
            5,
            "textDocument/formatting",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                ("options", JsonValue::obj(vec![])),
            ]),
        ),
        request(6, "shutdown", JsonValue::Null),
        notif("exit", JsonValue::Null),
    ]);

    // --- initialize result ---
    let init = by_id(&messages, 1);
    let caps = init
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .unwrap();
    assert_eq!(caps.get("hoverProvider"), Some(&JsonValue::Bool(true)));
    assert_eq!(caps.get("definitionProvider"), Some(&JsonValue::Bool(true)));
    assert_eq!(
        caps.get("documentFormattingProvider"),
        Some(&JsonValue::Bool(true))
    );
    let triggers = caps
        .get("completionProvider")
        .and_then(|c| c.get("triggerCharacters"))
        .and_then(|t| t.as_array())
        .unwrap();
    assert_eq!(triggers, &[JsonValue::str(".")]);

    // --- publishDiagnostics for the clean doc ---
    let publish = messages
        .iter()
        .find(|m| {
            m.get("method").and_then(|x| x.as_str()) == Some("textDocument/publishDiagnostics")
        })
        .expect("publishDiagnostics notification");
    let diags = publish
        .get("params")
        .and_then(|p| p.get("diagnostics"))
        .and_then(|d| d.as_array())
        .unwrap();
    assert!(diags.is_empty(), "clean document has no diagnostics");

    // --- hover returns a type ---
    let value = by_id(&messages, 2)
        .get("result")
        .and_then(|r| r.get("contents"))
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(value.contains("i32"), "hover type: {value}");

    // --- definition returns the right location ---
    let start = by_id(&messages, 3)
        .get("result")
        .and_then(|r| r.get("range"))
        .and_then(|r| r.get("start"))
        .unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));
    assert_eq!(start.get("character").and_then(|c| c.as_i64()), Some(0));

    // --- completion returns candidates ---
    let got = labels(by_id(&messages, 4).get("result").unwrap());
    assert!(got.contains(&"x".to_string()), "completion: {got:?}");
    assert!(got.contains(&"main".to_string()), "completion: {got:?}");

    // --- formatting returns the canonical text ---
    let edits = by_id(&messages, 5)
        .get("result")
        .and_then(|r| r.as_array())
        .unwrap();
    assert_eq!(edits.len(), 1);
    let new_text = edits[0].get("newText").and_then(|t| t.as_str()).unwrap();
    assert_eq!(new_text, k2_fmt::format_source(doc).unwrap());

    // --- shutdown acknowledged ---
    assert_eq!(by_id(&messages, 6).get("result"), Some(&JsonValue::Null));
}

#[test]
fn server_reports_diagnostics_on_a_broken_document() {
    let uri = "file:///broken.k2";
    let doc = "pub fn main() void {\n    const z = undeclared_name;\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        notif(
            "textDocument/didOpen",
            JsonValue::obj(vec![(
                "textDocument",
                JsonValue::obj(vec![
                    ("uri", JsonValue::str(uri)),
                    ("version", JsonValue::num(1)),
                    ("text", JsonValue::str(doc)),
                ]),
            )]),
        ),
        notif("exit", JsonValue::Null),
    ]);
    let publish = messages
        .iter()
        .find(|m| {
            m.get("method").and_then(|x| x.as_str()) == Some("textDocument/publishDiagnostics")
        })
        .expect("publishDiagnostics");
    let diags = publish
        .get("params")
        .and_then(|p| p.get("diagnostics"))
        .and_then(|d| d.as_array())
        .unwrap();
    assert!(!diags.is_empty(), "broken document yields diagnostics");
}

#[test]
fn large_integer_id_is_echoed_verbatim_by_the_server() {
    // A request id past 2^53 must come back bit-for-bit (JSON-RPC requires the
    // response id to equal the request id exactly). An `f64`-backed id would
    // round 9007199254740993 down to 9007199254740992.
    let big: i64 = 9_007_199_254_740_993;
    let shutdown = JsonValue::obj(vec![
        ("jsonrpc", JsonValue::str("2.0")),
        ("id", JsonValue::Int(big)),
        ("method", JsonValue::str("shutdown")),
        ("params", JsonValue::Null),
    ]);
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        shutdown,
        notif("exit", JsonValue::Null),
    ]);
    let reply = messages
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_i64()) == Some(big))
        .expect("response to the big-id shutdown");
    assert_eq!(reply.get("id"), Some(&JsonValue::Int(big)));
    assert_eq!(reply.get("result"), Some(&JsonValue::Null));
}

#[test]
fn unknown_request_gets_method_not_found() {
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        request(2, "textDocument/somethingUnknown", JsonValue::obj(vec![])),
        notif("exit", JsonValue::Null),
    ]);
    let err = by_id(&messages, 2);
    assert_eq!(
        err.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(crate::protocol::error_code::METHOD_NOT_FOUND)
    );
}

#[test]
fn request_before_initialize_is_rejected() {
    let messages = run_session(&[
        request(1, "textDocument/hover", td(uri_unused())),
        notif("exit", JsonValue::Null),
    ]);
    let err = by_id(&messages, 1);
    assert_eq!(
        err.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(crate::protocol::error_code::SERVER_NOT_INITIALIZED)
    );
}

/// A throwaway URI for the pre-initialize test.
fn uri_unused() -> &'static str {
    "file:///x.k2"
}

#[test]
fn close_clears_diagnostics() {
    let uri = "file:///c.k2";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        notif(
            "textDocument/didOpen",
            JsonValue::obj(vec![(
                "textDocument",
                JsonValue::obj(vec![
                    ("uri", JsonValue::str(uri)),
                    ("version", JsonValue::num(1)),
                    ("text", JsonValue::str("const x = ;\n")),
                ]),
            )]),
        ),
        notif("textDocument/didClose", td(uri)),
        notif("exit", JsonValue::Null),
    ]);
    // The last publishDiagnostics for this uri must be empty (cleared on close).
    let last = messages
        .iter()
        .rfind(|m| {
            m.get("method").and_then(|x| x.as_str()) == Some("textDocument/publishDiagnostics")
        })
        .expect("publishDiagnostics");
    let diags = last
        .get("params")
        .and_then(|p| p.get("diagnostics"))
        .and_then(|d| d.as_array())
        .unwrap();
    assert!(diags.is_empty(), "close clears diagnostics");
}
