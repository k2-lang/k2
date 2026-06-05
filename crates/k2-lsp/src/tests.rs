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
//  References
// =========================================================================

/// The number of `Location`s in a references result.
fn ref_count(result: &JsonValue) -> usize {
    result.as_array().map(|a| a.len()).unwrap_or(0)
}

/// The `(line, character)` of the start of the nth location in a references list.
fn ref_start(result: &JsonValue, n: usize) -> (i64, i64) {
    let loc = &result.as_array().unwrap()[n];
    let start = loc.get("range").and_then(|r| r.get("start")).unwrap();
    (
        start.get("line").and_then(|l| l.as_i64()).unwrap(),
        start.get("character").and_then(|c| c.as_i64()).unwrap(),
    )
}

#[test]
fn references_finds_all_occurrences() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let a = analyze(src);
    // Query on the first `x` use in `const y = x;`.
    let off = char_offset(src, "= x;") + 2;
    let refs = crate::features::references::compute(&a, "file:///t.k2", off, true);
    // The declaration `const x` plus the two `x` uses → 3 occurrences.
    assert_eq!(ref_count(&refs), 3, "decl + 2 uses: {refs:?}");
    // Sorted by position: the declaration (line 0) comes first.
    assert_eq!(ref_start(&refs, 0), (0, 6));
}

#[test]
fn references_excludes_declaration_when_requested() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= x;") + 2;
    let refs = crate::features::references::compute(&a, "u", off, false);
    // Only the two uses (declaration omitted).
    assert_eq!(ref_count(&refs), 2, "two uses only: {refs:?}");
}

#[test]
fn references_query_on_declaration_finds_all() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let a = analyze(src);
    // Query on the *declaration* `const x` name (offset 6).
    let off = char_offset(src, "const x") + 6;
    let refs = crate::features::references::compute(&a, "u", off, true);
    assert_eq!(ref_count(&refs), 3, "decl query finds all: {refs:?}");
}

#[test]
fn references_param_and_item_parity() {
    // A parameter: declaration + uses are all found.
    let src = "pub fn f(value: i32) i32 {\n    const a = value;\n    return value;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= value;") + 2;
    let refs = crate::features::references::compute(&a, "u", off, true);
    // param decl + 2 uses.
    assert_eq!(ref_count(&refs), 3, "param refs: {refs:?}");

    // A top-level item (function): declaration + call site.
    let src2 = "pub fn helper() void {}\npub fn main() void {\n    helper();\n}\n";
    let a2 = analyze(src2);
    let off2 = char_offset(src2, "    helper();") + 4;
    let refs2 = crate::features::references::compute(&a2, "u", off2, true);
    assert_eq!(ref_count(&refs2), 2, "item refs: {refs2:?}");
}

#[test]
fn references_on_whitespace_is_empty_array() {
    let src = "const x: i32 = 1;\n";
    let a = analyze(src);
    // Offset 0 is on the `const` keyword.
    let refs = crate::features::references::compute(&a, "u", 0, true);
    assert_eq!(refs, JsonValue::arr(Vec::new()));
}

// =========================================================================
//  Rename (+ prepareRename)
// =========================================================================

/// The `TextEdit[]` for `uri` in a rename `WorkspaceEdit`.
fn rename_edits<'a>(result: &'a JsonValue, uri: &str) -> &'a [JsonValue] {
    result
        .get("changes")
        .and_then(|c| c.get(uri))
        .and_then(|e| e.as_array())
        .unwrap_or(&[])
}

#[test]
fn rename_covers_all_occurrences() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let a = analyze(src);
    let uri = "file:///t.k2";
    let off = char_offset(src, "= x;") + 2;
    match crate::features::rename::compute(&a, uri, off, "w") {
        crate::features::rename::RenameOutcome::Edit(edit) => {
            let edits = rename_edits(&edit, uri);
            assert_eq!(edits.len(), 3, "3 edits: {edit:?}");
            for e in edits {
                assert_eq!(e.get("newText").and_then(|t| t.as_str()), Some("w"));
            }
        }
        _ => panic!("expected an edit"),
    }
}

#[test]
fn rename_rejects_invalid_name() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= x;") + 2;
    // A name starting with a digit is not a valid identifier.
    assert!(matches!(
        crate::features::rename::compute(&a, "u", off, "1bad"),
        crate::features::rename::RenameOutcome::InvalidName
    ));
    // A keyword is reserved and rejected.
    assert!(matches!(
        crate::features::rename::compute(&a, "u", off, "fn"),
        crate::features::rename::RenameOutcome::InvalidName
    ));
}

#[test]
fn rename_local_vs_param_vs_item() {
    // Local.
    let local = "pub fn main() void {\n    const a = 1;\n    const b = a;\n}\n";
    let a = analyze(local);
    let off = char_offset(local, "= a;") + 2;
    if let crate::features::rename::RenameOutcome::Edit(e) =
        crate::features::rename::compute(&a, "u", off, "renamed")
    {
        assert_eq!(rename_edits(&e, "u").len(), 2, "local decl + use");
    } else {
        panic!("local rename");
    }

    // Param.
    let param = "pub fn f(value: i32) i32 {\n    return value;\n}\n";
    let a = analyze(param);
    let off = char_offset(param, "return value") + 7;
    if let crate::features::rename::RenameOutcome::Edit(e) =
        crate::features::rename::compute(&a, "u", off, "v")
    {
        assert_eq!(rename_edits(&e, "u").len(), 2, "param decl + use");
    } else {
        panic!("param rename");
    }

    // Item (function).
    let item = "pub fn helper() void {}\npub fn main() void {\n    helper();\n}\n";
    let a = analyze(item);
    let off = char_offset(item, "    helper();") + 4;
    if let crate::features::rename::RenameOutcome::Edit(e) =
        crate::features::rename::compute(&a, "u", off, "helper2")
    {
        assert_eq!(rename_edits(&e, "u").len(), 2, "item decl + call");
    } else {
        panic!("item rename");
    }
}

#[test]
fn rename_on_predeclared_is_no_target() {
    let src = "const x: i32 = 1;\n";
    let a = analyze(src);
    let off = char_offset(src, "i32");
    assert!(matches!(
        crate::features::rename::compute(&a, "u", off, "MyInt"),
        crate::features::rename::RenameOutcome::NoTarget
    ));
}

#[test]
fn prepare_rename_returns_range_and_placeholder() {
    let src = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n}\n";
    let a = analyze(src);
    let off = char_offset(src, "= x;") + 2;
    let prep = crate::features::rename::prepare(&a, off);
    assert_eq!(prep.get("placeholder").and_then(|p| p.as_str()), Some("x"));
    // The range covers the `x` use on line 2.
    let start = prep.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(2));
}

#[test]
fn prepare_rename_on_keyword_is_null() {
    let src = "const x: i32 = 1;\n";
    let a = analyze(src);
    // Offset 0 is on the `const` keyword — not renameable.
    assert_eq!(crate::features::rename::prepare(&a, 0), JsonValue::Null);
    // The predeclared `i32` is likewise not renameable.
    let off = char_offset(src, "i32");
    assert_eq!(crate::features::rename::prepare(&a, off), JsonValue::Null);
}

#[test]
fn prepare_rename_on_field_and_enum_tag_is_null() {
    // A struct field's member-access uses (`p.x`) are `DeferredMember`, so a
    // rename could only touch the declaration and would corrupt the buffer. The
    // field declaration must therefore not be renameable: prepare → null, and
    // compute → NoTarget (not a partial edit).
    let field_src =
        "const Point = struct {\n    x: i32,\n    y: i32,\n};\npub fn f(p: Point) i32 {\n    return p.x + p.x;\n}\n";
    let a = analyze(field_src);
    // The resolver must have resolved this source so the field carries a real
    // DefId (the bug is precisely that the field IS resolvable yet not renameable).
    assert!(a.resolved.is_some(), "field source resolves");
    let field_off = char_offset(field_src, "    x: i32") + 4; // on the field name `x`
    assert_eq!(
        crate::features::rename::prepare(&a, field_off),
        JsonValue::Null,
        "field declaration is not renameable"
    );
    assert!(
        matches!(
            crate::features::rename::compute(&a, "u", field_off, "z"),
            crate::features::rename::RenameOutcome::NoTarget
        ),
        "field rename yields no target, not a partial edit"
    );
    // References on the field declaration must not falsely claim completeness:
    // the `p.x` uses are unrecoverable here, so report nothing rather than only
    // the declaration.
    let refs = crate::features::references::compute(&a, "u", field_off, true);
    assert_eq!(ref_count(&refs), 0, "no misleading single-hit references");

    // The same gate applies to an enum tag, whose `Color.red` use is likewise a
    // deferred member access.
    let enum_src = "const Color = enum {\n    red,\n    green,\n};\npub fn g() Color {\n    return Color.red;\n}\n";
    let a = analyze(enum_src);
    let tag_off = char_offset(enum_src, "    red,") + 4; // on the tag name `red`
    assert_eq!(
        crate::features::rename::prepare(&a, tag_off),
        JsonValue::Null,
        "enum tag declaration is not renameable"
    );
    assert!(
        matches!(
            crate::features::rename::compute(&a, "u", tag_off, "crimson"),
            crate::features::rename::RenameOutcome::NoTarget
        ),
        "enum tag rename yields no target"
    );

    // A local rename in the *same* program still covers ALL its occurrences, so
    // the gate is specific to deferred-member bindings and does not regress
    // local/param/item rename.
    let local_src =
        "pub fn main() void {\n    const a = 1;\n    const b = a;\n    const c = a;\n}\n";
    let a = analyze(local_src);
    let local_off = char_offset(local_src, "= a;") + 2; // on a use of `a`
    match crate::features::rename::compute(&a, "u", local_off, "renamed") {
        crate::features::rename::RenameOutcome::Edit(e) => {
            // Declaration + two uses = 3 edits, all renamed.
            assert_eq!(
                rename_edits(&e, "u").len(),
                3,
                "local rename covers all: {e:?}"
            );
        }
        _ => panic!("local rename should produce an edit, not a non-edit outcome"),
    }
}

// =========================================================================
//  Signature help
// =========================================================================

#[test]
fn signature_help_active_param() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    let a = analyze(src);
    // Cursor between `1,` and ` 2` → active parameter 1.
    let off = char_offset(src, "add(1, 2)") + 6; // just before `2`
    let help = crate::features::signature_help::compute(&a, off);
    assert_eq!(
        help.get("activeParameter").and_then(|p| p.as_i64()),
        Some(1),
        "{help:?}"
    );
    let sig = &help.get("signatures").and_then(|s| s.as_array()).unwrap()[0];
    let label = sig.get("label").and_then(|l| l.as_str()).unwrap();
    assert!(label.contains("a: i32"), "label: {label}");
    assert!(label.contains("b: i32"), "label: {label}");
    let params = sig.get("parameters").and_then(|p| p.as_array()).unwrap();
    assert_eq!(params.len(), 2);
}

#[test]
fn signature_help_first_param() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    let a = analyze(src);
    // Cursor right after `(` → active parameter 0.
    let off = char_offset(src, "add(1, 2)") + 4;
    let help = crate::features::signature_help::compute(&a, off);
    assert_eq!(
        help.get("activeParameter").and_then(|p| p.as_i64()),
        Some(0),
        "{help:?}"
    );
}

#[test]
fn signature_help_nested_call_depth_zero_commas() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(add(1, 2), 3);\n}\n";
    let a = analyze(src);
    // Cursor right before the outer `3` — the inner call's comma must not count.
    let off = char_offset(src, "add(1, 2), 3") + 11;
    let help = crate::features::signature_help::compute(&a, off);
    assert_eq!(
        help.get("activeParameter").and_then(|p| p.as_i64()),
        Some(1),
        "nested commas excluded: {help:?}"
    );
}

// =========================================================================
//  Inlay hints
// =========================================================================

/// The labels of an inlay-hint result.
fn hint_labels(result: &JsonValue) -> Vec<String> {
    result
        .as_array()
        .map(|hs| {
            hs.iter()
                .filter_map(|h| h.get("label").and_then(|l| l.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn inlay_hint_inferred_const_type() {
    let src = "pub fn main() void {\n    const a = 1;\n    const b: i32 = a;\n}\n";
    let a = analyze(src);
    let hints = crate::features::inlay_hint::compute(&a, 0, u32::MAX);
    let labels = hint_labels(&hints);
    // `a` (un-annotated) gets a `: <int>` hint; `b` (annotated) gets none.
    assert!(
        labels.iter().any(|l| l.starts_with(": ")),
        "an inferred-type hint: {labels:?}"
    );
    // Exactly one type hint (only `a`), so no second `: ` hint for `b`.
    let type_hints: Vec<&String> = labels.iter().filter(|l| l.starts_with(": ")).collect();
    assert_eq!(type_hints.len(), 1, "only `a` is annotated: {labels:?}");
}

#[test]
fn inlay_hint_parameter_names() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    let a = analyze(src);
    let hints = crate::features::inlay_hint::compute(&a, 0, u32::MAX);
    let labels = hint_labels(&hints);
    assert!(
        labels.contains(&"a:".to_string()),
        "param hint a: {labels:?}"
    );
    assert!(
        labels.contains(&"b:".to_string()),
        "param hint b: {labels:?}"
    );
}

#[test]
fn inlay_hint_range_filters() {
    let src = "pub fn main() void {\n    const a = 1;\n    const c = 2;\n}\n";
    let a = analyze(src);
    // Restrict the range to only the first declaration's line.
    let lo = char_offset(src, "const a");
    let hi = char_offset(src, "const c");
    let hints = crate::features::inlay_hint::compute(&a, lo, hi);
    let labels = hint_labels(&hints);
    // Only `a`'s hint anchored before `const c`.
    let type_hints: Vec<&String> = labels.iter().filter(|l| l.starts_with(": ")).collect();
    assert_eq!(type_hints.len(), 1, "range filter: {labels:?}");
}

// =========================================================================
//  Semantic tokens
// =========================================================================

/// One decoded absolute semantic token.
#[derive(Debug)]
struct DecodedToken {
    line: u32,
    character: u32,
    length: u32,
    ty: u32,
    modifiers: u32,
}

/// Decodes the LSP delta `data` array back to absolute tokens.
fn decode_tokens(result: &JsonValue) -> Vec<DecodedToken> {
    let data = result.get("data").and_then(|d| d.as_array()).unwrap();
    let nums: Vec<u32> = data.iter().map(|v| v.as_i64().unwrap() as u32).collect();
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut character = 0u32;
    for chunk in nums.chunks(5) {
        let (dl, dc, len, ty, m) = (chunk[0], chunk[1], chunk[2], chunk[3], chunk[4]);
        if dl == 0 {
            character += dc;
        } else {
            line += dl;
            character = dc;
        }
        out.push(DecodedToken {
            line,
            character,
            length: len,
            ty,
            modifiers: m,
        });
    }
    out
}

/// The legend index of a token-type name.
fn ttype(name: &str) -> u32 {
    crate::protocol::token_type_index(name)
}

#[test]
fn semantic_tokens_non_empty_and_classified() {
    let src = "// a comment\npub fn add(a: i32) i32 {\n    return a + 1;\n}\n";
    let a = analyze(src);
    let result = crate::features::semantic_tokens::compute(&a);
    let data = result.get("data").and_then(|d| d.as_array()).unwrap();
    assert!(!data.is_empty(), "non-empty token list");
    assert_eq!(data.len() % 5, 0, "length multiple of 5");

    let toks = decode_tokens(&result);
    // Strictly non-decreasing in (line, character).
    for w in toks.windows(2) {
        assert!(
            (w[0].line, w[0].character) <= (w[1].line, w[1].character),
            "monotonic: {:?} then {:?}",
            w[0],
            w[1]
        );
    }

    // The comment on line 0 is classified `comment`.
    let comment = toks.iter().find(|t| t.line == 0).unwrap();
    assert_eq!(comment.ty, ttype("comment"), "line-0 comment: {comment:?}");

    // `pub` / `fn` on line 1 are keywords.
    let kw = toks
        .iter()
        .filter(|t| t.line == 1 && t.ty == ttype("keyword"))
        .count();
    assert!(kw >= 2, "pub and fn are keywords");

    // The function name `add` is a function with the declaration modifier.
    let func = toks
        .iter()
        .find(|t| t.line == 1 && t.ty == ttype("function"))
        .expect("fn name token");
    let decl_bit = 1u32 << crate::protocol::token_modifier_index("declaration");
    assert!(
        func.modifiers & decl_bit != 0,
        "fn name has declaration modifier: {func:?}"
    );
    // The fn name `add` is 3 UTF-16 code units long.
    assert_eq!(func.length, 3, "fn name length: {func:?}");

    // A parameter use on line 2 (`return a`).
    assert!(
        toks.iter().any(|t| t.ty == ttype("parameter")),
        "a parameter token exists"
    );
    // The int literal `1` is a number.
    assert!(
        toks.iter().any(|t| t.ty == ttype("number")),
        "a number token exists"
    );
}

#[test]
fn semantic_tokens_string_literal_classified() {
    let src = "pub fn main() void {\n    const s = \"hello\";\n}\n";
    let a = analyze(src);
    let toks = decode_tokens(&crate::features::semantic_tokens::compute(&a));
    assert!(
        toks.iter().any(|t| t.ty == ttype("string")),
        "a string token exists: {toks:?}"
    );
}

#[test]
fn semantic_tokens_doc_comment_emitted_exactly_once() {
    // A `///` doc comment is recorded by the lexer in both the token channel
    // (`TokenKind::DocComment`) and the trivia channel (`TriviaKind::DocComment`).
    // It must yield exactly ONE `comment` semantic token; a second, identically
    // positioned token would overlap (deltaLine=0, deltaChar=0, length>0), which
    // the LSP semantic-tokens spec forbids.
    let src = "/// docs\npub fn f() void {}\n";
    let a = analyze(src);
    let toks = decode_tokens(&crate::features::semantic_tokens::compute(&a));

    // Exactly one comment token, and it covers the whole `/// docs` line.
    let comments: Vec<&DecodedToken> = toks.iter().filter(|t| t.ty == ttype("comment")).collect();
    assert_eq!(comments.len(), 1, "one comment token for `///`: {toks:?}");
    let c = comments[0];
    assert_eq!((c.line, c.character), (0, 0), "comment at line 0: {c:?}");
    assert_eq!(c.length, 8, "`/// docs` is 8 UTF-16 units: {c:?}");

    // No two tokens overlap: a token never starts before the previous one ends.
    // Same-line: prev.character + prev.length <= next.character. Earlier line: ok.
    for w in toks.windows(2) {
        let (p, n) = (&w[0], &w[1]);
        // Sorted, non-negative deltas: monotonic in (line, character).
        assert!(
            (p.line, p.character) <= (n.line, n.character),
            "non-negative delta: {p:?} then {n:?}"
        );
        if p.line == n.line {
            assert!(
                p.character + p.length <= n.character,
                "no overlap on a line: {p:?} then {n:?}"
            );
        }
    }
}

// =========================================================================
//  Code actions
// =========================================================================

#[test]
fn code_action_applies_suggestion() {
    let src = "pub fn main() void {\n    const value = 1;\n    const z = valu;\n}\n";
    let a = analyze(src);
    let uri = "file:///t.k2";
    let lo = char_offset(src, "valu;");
    let hi = lo + 4;
    let actions = crate::features::code_action::compute(&a, uri, lo, hi);
    let arr = actions.as_array().unwrap();
    assert!(!arr.is_empty(), "a quick-fix is offered: {actions:?}");
    let action = &arr[0];
    assert_eq!(
        action.get("title").and_then(|t| t.as_str()),
        Some("Change to `value`")
    );
    assert_eq!(
        action.get("kind").and_then(|k| k.as_str()),
        Some("quickfix")
    );
    let edit = action
        .get("edit")
        .and_then(|e| e.get("changes"))
        .and_then(|c| c.get(uri))
        .and_then(|e| e.as_array())
        .unwrap();
    assert_eq!(
        edit[0].get("newText").and_then(|t| t.as_str()),
        Some("value")
    );

    // Apply the edit and re-analyze: the typo is fixed.
    let fixed = src.replace("const z = valu;", "const z = value;");
    let fixed_a = analyze(&fixed);
    let diags = crate::features::diagnostics::compute(&fixed_a);
    assert_eq!(
        diags.as_array().map(|a| a.len()),
        Some(0),
        "the suggestion actually fixes the error"
    );
}

// =========================================================================
//  Cross-file definition + references
// =========================================================================

#[test]
fn cross_file_definition_path_import() {
    use crate::document::DocumentStore;
    let dir = std::env::temp_dir().join(format!("k2lsp_xfile_def_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let a_path = dir.join("a.k2");
    let b_path = dir.join("b.k2");
    let a_uri = crate::workspace::path_to_uri(&a_path);
    let b_uri = crate::workspace::path_to_uri(&b_path);
    let a_src = "const b = @import(\"./b.k2\");\npub fn main() void {\n    const r = b.foo();\n}\n";
    let b_src = "pub fn foo() i32 {\n    return 1;\n}\n";
    std::fs::write(&a_path, a_src).unwrap();
    std::fs::write(&b_path, b_src).unwrap();

    let mut store = DocumentStore::new();
    store.open(a_uri.clone(), 1, a_src.to_string());
    {
        let doc = store.get_mut(&a_uri).unwrap();
        let _ = doc.analysis();
    }
    let analysis = store.get(&a_uri).unwrap().cached_analysis().unwrap();
    // Offset of `foo` in `b.foo()`.
    let off = char_offset(a_src, "b.foo()") + 2;
    let def = crate::workspace::cross_file_definition(&store, analysis, &a_uri, off)
        .expect("cross-file definition");
    assert_eq!(
        def.get("uri").and_then(|u| u.as_str()),
        Some(b_uri.as_str())
    );
    // `foo` is declared on line 0 of b.k2.
    let start = def.get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(|l| l.as_i64()), Some(0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cross_file_references_path_import() {
    use crate::document::DocumentStore;
    let dir = std::env::temp_dir().join(format!("k2lsp_xfile_ref_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let a_path = dir.join("a.k2");
    let b_path = dir.join("b.k2");
    let a_uri = crate::workspace::path_to_uri(&a_path);
    let b_uri = crate::workspace::path_to_uri(&b_path);
    let a_src = "const b = @import(\"./b.k2\");\npub fn main() void {\n    const r = b.foo();\n}\n";
    let b_src = "pub fn foo() i32 {\n    return 1;\n}\n";
    std::fs::write(&a_path, a_src).unwrap();
    std::fs::write(&b_path, b_src).unwrap();

    let mut store = DocumentStore::new();
    store.open(a_uri.clone(), 1, a_src.to_string());
    store.open(b_uri.clone(), 1, b_src.to_string());

    // References of `foo` queried from b.k2 include the use in a.k2.
    let extra = crate::workspace::cross_file_references(&store, &b_uri, "foo");
    assert!(
        extra
            .iter()
            .any(|loc| loc.get("uri").and_then(|u| u.as_str()) == Some(a_uri.as_str())),
        "a.k2 use of b.foo is found: {extra:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
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

// =========================================================================
//  Scripted sessions for the v0.26 features
// =========================================================================

/// Builds a `didOpen` notification for `uri` with `text`.
fn open_doc(uri: &str, text: &str) -> JsonValue {
    notif(
        "textDocument/didOpen",
        JsonValue::obj(vec![(
            "textDocument",
            JsonValue::obj(vec![
                ("uri", JsonValue::str(uri)),
                ("languageId", JsonValue::str("k2")),
                ("version", JsonValue::num(1)),
                ("text", JsonValue::str(text)),
            ]),
        )]),
    )
}

#[test]
fn initialize_advertises_new_capabilities() {
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        notif("exit", JsonValue::Null),
    ]);
    let caps = by_id(&messages, 1)
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .unwrap();
    assert_eq!(caps.get("referencesProvider"), Some(&JsonValue::Bool(true)));
    assert_eq!(caps.get("inlayHintProvider"), Some(&JsonValue::Bool(true)));
    assert_eq!(
        caps.get("renameProvider")
            .and_then(|r| r.get("prepareProvider")),
        Some(&JsonValue::Bool(true))
    );
    let st = caps.get("semanticTokensProvider").unwrap();
    let types = st
        .get("legend")
        .and_then(|l| l.get("tokenTypes"))
        .and_then(|t| t.as_array())
        .unwrap();
    assert!(!types.is_empty(), "non-empty semantic token legend");
    assert_eq!(st.get("full"), Some(&JsonValue::Bool(true)));
    let sh = caps.get("signatureHelpProvider").unwrap();
    let triggers = sh
        .get("triggerCharacters")
        .and_then(|t| t.as_array())
        .unwrap();
    assert!(triggers.contains(&JsonValue::str("(")));
    let ca = caps.get("codeActionProvider").unwrap();
    assert!(ca
        .get("codeActionKinds")
        .and_then(|k| k.as_array())
        .map(|k| k.contains(&JsonValue::str("quickfix")))
        .unwrap_or(false));
}

#[test]
fn session_references() {
    let uri = "file:///refs.k2";
    let doc = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        // Reference query on the `x` use at line 2 (`const y = x;`), char 14.
        request(
            2,
            "textDocument/references",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                ("position", pos(2, 14)),
                (
                    "context",
                    JsonValue::obj(vec![("includeDeclaration", JsonValue::Bool(true))]),
                ),
            ]),
        ),
        notif("exit", JsonValue::Null),
    ]);
    let result = by_id(&messages, 2).get("result").unwrap();
    assert_eq!(result.as_array().map(|a| a.len()), Some(3), "{result:?}");
}

#[test]
fn session_rename_ok_and_bad_name() {
    let uri = "file:///rename.k2";
    let doc = "const x: i32 = 1;\npub fn main() void {\n    const y = x;\n    const z = x;\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        request(
            2,
            "textDocument/rename",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                ("position", pos(2, 14)),
                ("newName", JsonValue::str("w")),
            ]),
        ),
        request(
            3,
            "textDocument/rename",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                ("position", pos(2, 14)),
                ("newName", JsonValue::str("1bad")),
            ]),
        ),
        request(4, "textDocument/prepareRename", td_pos(uri, pos(2, 14))),
        notif("exit", JsonValue::Null),
    ]);
    // Rename → 3 edits.
    let edits = by_id(&messages, 2)
        .get("result")
        .and_then(|r| r.get("changes"))
        .and_then(|c| c.get(uri))
        .and_then(|e| e.as_array())
        .unwrap();
    assert_eq!(edits.len(), 3);
    // Bad name → InvalidParams error.
    assert_eq!(
        by_id(&messages, 3)
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(crate::protocol::error_code::INVALID_PARAMS)
    );
    // prepareRename → placeholder `x`.
    assert_eq!(
        by_id(&messages, 4)
            .get("result")
            .and_then(|r| r.get("placeholder"))
            .and_then(|p| p.as_str()),
        Some("x")
    );
}

#[test]
fn session_signature_help() {
    let uri = "file:///sig.k2";
    let doc = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    // The `2` argument is at line 2; `add(1, 2)` starts at char 14, so `2` is at
    // char 21.
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        request(2, "textDocument/signatureHelp", td_pos(uri, pos(2, 21))),
        notif("exit", JsonValue::Null),
    ]);
    let result = by_id(&messages, 2).get("result").unwrap();
    assert_eq!(
        result.get("activeParameter").and_then(|p| p.as_i64()),
        Some(1),
        "{result:?}"
    );
}

#[test]
fn session_inlay_hint() {
    let uri = "file:///inlay.k2";
    let doc = "pub fn main() void {\n    const a = 1;\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        request(
            2,
            "textDocument/inlayHint",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                (
                    "range",
                    JsonValue::obj(vec![("start", pos(0, 0)), ("end", pos(3, 0))]),
                ),
            ]),
        ),
        notif("exit", JsonValue::Null),
    ]);
    let result = by_id(&messages, 2).get("result").unwrap();
    let hints = result.as_array().unwrap();
    assert!(
        hints.iter().any(|h| h
            .get("label")
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .starts_with(": ")),
        "an inferred-type hint: {result:?}"
    );
}

#[test]
fn session_semantic_tokens() {
    let uri = "file:///sem.k2";
    let doc = "pub fn add(a: i32) i32 {\n    return a + 1;\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        request(2, "textDocument/semanticTokens/full", td(uri)),
        notif("exit", JsonValue::Null),
    ]);
    let data = by_id(&messages, 2)
        .get("result")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.as_array())
        .unwrap();
    assert!(!data.is_empty(), "non-empty token data");
    assert_eq!(data.len() % 5, 0, "data length is a multiple of 5");
}

#[test]
fn session_code_action() {
    let uri = "file:///ca.k2";
    let doc = "pub fn main() void {\n    const value = 1;\n    const z = valu;\n}\n";
    // `valu` is on line 2; `const z = ` is 14 chars, so `valu` at char 14..18.
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        request(
            2,
            "textDocument/codeAction",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                (
                    "range",
                    JsonValue::obj(vec![("start", pos(2, 14)), ("end", pos(2, 18))]),
                ),
                (
                    "context",
                    JsonValue::obj(vec![("diagnostics", JsonValue::arr(Vec::new()))]),
                ),
            ]),
        ),
        notif("exit", JsonValue::Null),
    ]);
    let result = by_id(&messages, 2).get("result").unwrap();
    let actions = result.as_array().unwrap();
    assert!(!actions.is_empty(), "a code action: {result:?}");
    let new_text = actions[0]
        .get("edit")
        .and_then(|e| e.get("changes"))
        .and_then(|c| c.get(uri))
        .and_then(|e| e.as_array())
        .and_then(|e| e.first())
        .and_then(|e| e.get("newText"))
        .and_then(|t| t.as_str());
    assert_eq!(new_text, Some("value"));
}

#[test]
fn scripted_session_drives_every_new_feature() {
    let uri = "file:///all_new.k2";
    let doc = "pub fn add(a: i32, b: i32) i32 { return a; }\npub fn main() void {\n    const r = add(1, 2);\n}\n";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(uri, doc),
        // references on the `add` call (line 2, char 14).
        request(
            2,
            "textDocument/references",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                ("position", pos(2, 14)),
                (
                    "context",
                    JsonValue::obj(vec![("includeDeclaration", JsonValue::Bool(true))]),
                ),
            ]),
        ),
        request(3, "textDocument/signatureHelp", td_pos(uri, pos(2, 21))),
        request(4, "textDocument/semanticTokens/full", td(uri)),
        request(
            5,
            "textDocument/inlayHint",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(uri))]),
                ),
                (
                    "range",
                    JsonValue::obj(vec![("start", pos(0, 0)), ("end", pos(4, 0))]),
                ),
            ]),
        ),
        request(6, "shutdown", JsonValue::Null),
        notif("exit", JsonValue::Null),
    ]);
    // references: `add` declaration + call site → 2.
    assert_eq!(
        by_id(&messages, 2)
            .get("result")
            .and_then(|r| r.as_array())
            .map(|a| a.len()),
        Some(2)
    );
    // signatureHelp: active param 1.
    assert_eq!(
        by_id(&messages, 3)
            .get("result")
            .and_then(|r| r.get("activeParameter"))
            .and_then(|p| p.as_i64()),
        Some(1)
    );
    // semanticTokens: non-empty.
    assert!(!by_id(&messages, 4)
        .get("result")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.as_array())
        .unwrap()
        .is_empty());
    // inlayHint: param-name hints present.
    assert!(by_id(&messages, 5)
        .get("result")
        .and_then(|r| r.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false));
    assert_eq!(by_id(&messages, 6).get("result"), Some(&JsonValue::Null));
}

#[test]
fn every_new_request_on_empty_or_broken_doc_is_safe() {
    // An empty doc, a non-parsing doc, and an unopened URI — every new request
    // must yield null/empty/error and never panic, and the session must complete.
    let empty_uri = "file:///empty.k2";
    let broken_uri = "file:///broken2.k2";
    let messages = run_session(&[
        request(1, "initialize", JsonValue::obj(vec![])),
        notif("initialized", JsonValue::obj(vec![])),
        open_doc(empty_uri, ""),
        open_doc(broken_uri, "pub fn ("),
        // References at a position past EOF in the empty doc.
        request(
            2,
            "textDocument/references",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(empty_uri))]),
                ),
                ("position", pos(99, 99)),
                ("context", JsonValue::obj(vec![])),
            ]),
        ),
        // Rename with a missing newName on the broken doc.
        request(
            3,
            "textDocument/rename",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(broken_uri))]),
                ),
                ("position", pos(0, 5)),
            ]),
        ),
        // signatureHelp / inlayHint / semanticTokens / codeAction / prepareRename
        // on the broken doc and an unopened URI.
        request(
            4,
            "textDocument/signatureHelp",
            td_pos(broken_uri, pos(0, 3)),
        ),
        request(
            5,
            "textDocument/inlayHint",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str("file:///nope.k2"))]),
                ),
                (
                    "range",
                    JsonValue::obj(vec![("start", pos(0, 0)), ("end", pos(1, 0))]),
                ),
            ]),
        ),
        request(6, "textDocument/semanticTokens/full", td(broken_uri)),
        request(
            7,
            "textDocument/codeAction",
            JsonValue::obj(vec![
                (
                    "textDocument",
                    JsonValue::obj(vec![("uri", JsonValue::str(broken_uri))]),
                ),
                (
                    "range",
                    JsonValue::obj(vec![("start", pos(0, 0)), ("end", pos(0, 5))]),
                ),
                ("context", JsonValue::obj(vec![])),
            ]),
        ),
        request(
            8,
            "textDocument/prepareRename",
            td_pos(broken_uri, pos(0, 3)),
        ),
        // A request with no doc at all (unopened).
        request(
            9,
            "textDocument/references",
            td_pos("file:///gone.k2", pos(0, 0)),
        ),
        request(10, "shutdown", JsonValue::Null),
        notif("exit", JsonValue::Null),
    ]);
    // Every request got a response (the server kept serving, never panicked).
    for id in 2..=10 {
        let m = by_id(&messages, id);
        assert!(
            m.get("result").is_some() || m.get("error").is_some(),
            "request {id} got a response: {m:?}"
        );
    }
    // The final shutdown is acknowledged with null.
    assert_eq!(by_id(&messages, 10).get("result"), Some(&JsonValue::Null));
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
