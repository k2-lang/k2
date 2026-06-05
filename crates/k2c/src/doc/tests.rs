//! Unit tests for the doc MODEL builder: signature extraction (from the type
//! checker), pub-field filtering, container namespacing, the degraded (type-error)
//! path, and the never-panic guarantee over the example corpus.

use super::*;

/// Finds an item by name across every module of a model.
fn find_item<'a>(model: &'a DocModel, name: &str) -> Option<&'a DocItem> {
    model
        .modules
        .iter()
        .flat_map(|m| m.items.iter())
        .find(|i| i.name == name)
}

#[test]
fn function_signature_from_type_checker() {
    let src = "pub fn add(a: i32, b: i32) i32 { return a + b; }";
    let model = build_doc_model(src, "test.k2").expect("model");
    let add = find_item(&model, "add").expect("add documented");
    assert_eq!(add.kind, DocKind::Fn);
    assert_eq!(add.signature, "pub fn add(a: i32, b: i32) i32");
    assert_eq!(add.params.len(), 2);
    assert_eq!(add.params[0].name, "a");
    assert_eq!(add.params[0].ty, "i32");
    assert_eq!(add.ret.as_deref(), Some("i32"));
}

#[test]
fn generic_signature_shows_type_param_not_deferred() {
    // A generic free fn whose param/return types mention `comptime T: type` must
    // render `T`/`?T`/`[]T`, NOT the internal `deferred` placeholder.
    let src = "pub fn id(comptime T: type, x: T) T { return x; }";
    let model = build_doc_model(src, "t.k2").expect("model");
    let id = find_item(&model, "id").expect("id documented");
    assert_eq!(id.signature, "pub fn id(comptime T: type, x: T) T");
    assert!(
        !id.signature.contains("deferred"),
        "deferred leaked: {}",
        id.signature
    );
    assert_eq!(id.params[1].ty, "T", "param type should be T");
    assert_eq!(id.ret.as_deref(), Some("T"), "return type should be T");

    // `?T` and `[]T` variants also render the source spelling.
    let opt = build_doc_model("pub fn maybe(comptime T: type) ?T { return null; }", "t.k2")
        .expect("model");
    let maybe = find_item(&opt, "maybe").expect("maybe");
    assert_eq!(maybe.ret.as_deref(), Some("?T"), "{}", maybe.signature);

    // A real type whose name merely CONTAINS "deferred" is not rewritten.
    assert!(contains_deferred_token("deferred"));
    assert!(contains_deferred_token("?deferred"));
    assert!(contains_deferred_token("[]deferred"));
    assert!(!contains_deferred_token("DeferredQueue"));
    assert!(!contains_deferred_token("deferred_count"));
}

#[test]
fn const_signature_has_type() {
    let src = "pub const MAX: u32 = 100;";
    let model = build_doc_model(src, "t.k2").expect("model");
    let max = find_item(&model, "MAX").expect("MAX documented");
    assert_eq!(max.kind, DocKind::Const);
    assert!(
        max.signature.starts_with("const MAX: u32"),
        "{}",
        max.signature
    );
}

#[test]
fn struct_fields_are_pub_filtered() {
    let src = "pub const Point = struct { pub x: i32, y: i32 };";
    let model = build_doc_model(src, "t.k2").expect("model");
    let point = find_item(&model, "Point").expect("Point documented");
    assert_eq!(point.kind, DocKind::Struct);
    // Public field present; private (no doc) excluded.
    let names: Vec<&str> = point.fields.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"x"), "pub field x missing: {names:?}");
    assert!(!names.contains(&"y"), "private field y leaked: {names:?}");
    // A fields-only struct needs no separate namespace page (its fields render
    // inline on the item), so no `Point` module is created.
    assert!(
        !model
            .modules
            .iter()
            .any(|m| m.path == vec!["Point".to_string()]),
        "fields-only struct should not spin up a module"
    );
}

#[test]
fn container_with_method_yields_namespace_module() {
    let src = "pub const Point = struct {\n\
               pub x: i32,\n\
               /// Origin.\n\
               pub fn origin() Point { return Point{ .x = 0 }; }\n\
               };";
    let model = build_doc_model(src, "t.k2").expect("model");
    // The container with a documented method gets its own namespace page holding
    // the method as an item.
    let module = model
        .modules
        .iter()
        .find(|m| m.path == vec!["Point".to_string()])
        .expect("Point module for the method");
    assert!(
        module.items.iter().any(|i| i.name == "origin"),
        "origin method missing from Point module"
    );
}

#[test]
fn enum_variants_documented() {
    let src = "pub const Color = enum { Red, Green, Blue };";
    let model = build_doc_model(src, "t.k2").expect("model");
    let color = find_item(&model, "Color").expect("Color documented");
    assert_eq!(color.kind, DocKind::Enum);
    let names: Vec<&str> = color.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["Red", "Green", "Blue"]);
}

#[test]
fn doc_comment_text_is_attached() {
    let src = "/// Adds two numbers.\npub fn add(a: i32, b: i32) i32 { return a + b; }";
    let model = build_doc_model(src, "t.k2").expect("model");
    let add = find_item(&model, "add").unwrap();
    assert!(add.doc_md.contains("Adds two numbers."), "{}", add.doc_md);
}

#[test]
fn trailing_file_level_doc_is_captured() {
    // A trailing `///` block (no `//!` syntax in k2) populates `SourceFile.doc`. The
    // std prelude is appended AFTER the user source by `parse_program`, so reading
    // the file doc from the std-injected AST would lose it; the model must capture
    // it from the raw user AST.
    let src = "pub fn f() void {}\n/// A trailing file-level doc note.";
    let model = build_doc_model(src, "t.k2").expect("model");
    assert!(
        model.file_doc.contains("trailing file-level doc note"),
        "file doc lost: {:?}",
        model.file_doc
    );

    // A file that is ONLY a trailing doc (no items) still yields the doc.
    let only = build_doc_model("/// Only a trailing doc, no items.", "t.k2").expect("model");
    assert!(only.file_doc.contains("Only a trailing doc"));
}

#[test]
fn file_level_doc_examples_are_collected() {
    // A fenced example in the file-level doc must be collected so it renders AND is
    // run/gated by the doc-test runner.
    let src = "pub fn f() void {}\n/// Trailing.\n/// ```k2\n/// const x = 1;\n/// ```";
    let model = build_doc_model(src, "t.k2").expect("model");
    assert_eq!(
        model.file_examples.len(),
        1,
        "file-level example not collected"
    );
}

#[test]
fn examples_extracted_from_doc() {
    let src = "/// Doc.\n///\n/// ```k2\n/// const x = 1;\n/// ```\npub fn f() void {}";
    let model = build_doc_model(src, "t.k2").expect("model");
    let f = find_item(&model, "f").unwrap();
    assert_eq!(f.examples.len(), 1);
}

#[test]
fn degraded_path_on_type_error_does_not_panic() {
    // A genuine type error in the body; the file still parses, so a model must come
    // out (no panic), with `add` present and a sensible signature.
    let src = "pub fn add(a: i32, b: i32) i32 { return a + undefinedName; }";
    let model = build_doc_model(src, "t.k2").expect("model even with type error");
    let add = find_item(&model, "add").expect("add still documented");
    assert!(add.signature.contains("fn add"), "{}", add.signature);
}

#[test]
fn private_documented_item_is_included() {
    // A non-pub fn WITH a doc comment is documented (rustdoc-private-items style).
    let src = "/// Helper.\nfn helper() void {}";
    let model = build_doc_model(src, "t.k2").expect("model");
    assert!(
        find_item(&model, "helper").is_some(),
        "documented private item dropped"
    );
}

#[test]
fn private_undocumented_item_is_excluded() {
    let src = "fn helper() void {}";
    let model = build_doc_model(src, "t.k2").expect("model");
    assert!(
        find_item(&model, "helper").is_none(),
        "private undocumented item leaked"
    );
}

#[test]
fn std_root_is_never_documented() {
    let src = "const std = @import(\"std\");\npub fn f() void {}";
    let model = build_doc_model(src, "t.k2").expect("model");
    assert!(
        find_item(&model, k2_std::STD_ROOT_NAME).is_none(),
        "std root leaked into docs"
    );
}

#[test]
fn empty_file_yields_empty_model_no_panic() {
    let model = build_doc_model("", "empty.k2").expect("empty model");
    // Just the root module, no items.
    assert!(model.modules.iter().all(|m| m.items.is_empty()));
}

#[test]
fn malformed_markdown_does_not_panic() {
    let src =
        "/// ```k2\n/// unterminated fence and ` stray backtick and [bad](\npub fn f() void {}";
    let model = build_doc_model(src, "t.k2").expect("model");
    // Rendering must also be total.
    let pages = render::emit_html(&model);
    assert!(!pages.is_empty());
    for p in &pages {
        render::assert_balanced(&p.contents).expect("balanced HTML");
    }
}

#[test]
fn html_output_is_well_formed_and_self_contained() {
    let src = "/// A point in 2D.\npub const Point = struct { pub x: i32, pub y: i32 };\n\
               /// Adds.\npub fn add(a: i32, b: i32) i32 { return a + b; }";
    let model = build_doc_model(src, "t.k2").expect("model");
    let pages = render::emit_html(&model);
    let index = pages.iter().find(|p| p.filename == "index.html").unwrap();
    assert!(index.contents.contains("<!DOCTYPE html>"));
    assert!(
        index.contents.contains("<style>"),
        "no inline CSS (not self-contained)"
    );
    assert!(
        !index.contents.contains("http://"),
        "external resource referenced"
    );
    assert!(index.contents.contains("fn add(a: i32, b: i32) i32"));
    render::assert_balanced(&index.contents).expect("balanced index");
    for p in &pages {
        render::assert_balanced(&p.contents).expect("balanced page");
    }
}

#[test]
fn markdown_table_cells_escape_pipes() {
    // A field whose DOC contains a `|` (e.g. a `code|span`) must not corrupt the GFM
    // table: the `|` is escaped so the row keeps its column count.
    let src = "pub const S = struct {\n\
               /// Doc with a | pipe and a `code|span`.\n\
               pub x: u64,\n\
               };";
    let model = build_doc_model(src, "t.k2").expect("model");
    let pages = render::emit_markdown(&model);
    let all: String = pages.iter().map(|p| p.contents.as_str()).collect();
    // The table row for `x` must carry an ESCAPED pipe, never a bare one in the doc
    // cell that would split the row.
    let row = all
        .lines()
        .find(|l| l.contains("`x`") && l.contains("pipe"))
        .expect("field row for x");
    assert!(
        row.contains("\\|"),
        "pipe not escaped in md table cell: {row}"
    );
    // The row has exactly 3 data cells (4 `|` delimiters): leading, 2 separators,
    // trailing — any UNescaped `|` would add more.
    let bare_pipes = row.match_indices('|').filter(|(i, _)| {
        // A `|` preceded by a backslash is escaped (part of `\|`), not a delimiter.
        *i == 0 || row.as_bytes()[i - 1] != b'\\'
    });
    assert_eq!(
        bare_pipes.count(),
        4,
        "row has extra unescaped column delimiters: {row}"
    );
}

#[test]
fn ast_type_string_is_total() {
    use k2_syntax::{Expr, Span};
    let id = Expr::ident("Foo", Span::default());
    assert_eq!(ast_type_string(&id), "Foo");
    let opt = Expr::Optional {
        inner: Box::new(Expr::ident("u8", Span::default())),
        span: Span::default(),
    };
    assert_eq!(ast_type_string(&opt), "?u8");
}

#[test]
fn directory_mode_links_resolve_to_written_files() {
    // Two files documented in directory mode get distinct `{slug}--` prefixes. Every
    // intra-site href in a prefixed page MUST resolve to a page actually emitted for
    // that file (the C-cluster bug emitted un-prefixed URLs that 404'd).
    let src = "/// Origin.\npub const origin: i32 = 0;\n\
               /// A point.\npub const Point = struct {\n\
               pub x: i32,\n\
               /// Add.\npub fn add(self: Point, d: i32) i32 { return self.x + d; }\n\
               };";
    let model = build_doc_model(src, "/tmp/a.k2").expect("model");
    let prefix = format!("{}--", render::slug(&model.label));
    let pages = render::emit_html_prefixed(&model, &prefix);

    // The set of emitted filenames.
    let names: std::collections::HashSet<&str> =
        pages.iter().map(|p| p.filename.as_str()).collect();
    // Every emitted filename carries the prefix.
    for p in &pages {
        assert!(
            p.filename.starts_with(&prefix),
            "page filename missing prefix: {}",
            p.filename
        );
    }

    // Collect every `href="..."` target across all pages; the file part (before a
    // `#anchor`) must be either a pure in-page anchor or a written filename.
    for p in &pages {
        for href in extract_hrefs(&p.contents) {
            let file = href.split('#').next().unwrap_or("");
            if file.is_empty() {
                continue; // pure `#anchor`
            }
            assert!(
                names.contains(file),
                "href `{href}` in `{}` targets a file that was not written; emitted: {names:?}",
                p.filename
            );
        }
    }
}

/// Extracts every `href="..."` attribute value from an HTML string (test helper).
fn extract_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes: Vec<char> = html.chars().collect();
    let needle: Vec<char> = "href=\"".chars().collect();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()] == needle[..] {
            let start = i + needle.len();
            if let Some(end) = (start..bytes.len()).find(|&j| bytes[j] == '"') {
                out.push(bytes[start..end].iter().collect());
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The never-panic fuzz over the real example corpus: documenting every
/// `examples/*.k2` must produce a model and balanced HTML without panicking.
#[test]
fn corpus_never_panics() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples"));
    let dir = match dir {
        Some(d) if d.is_dir() => d,
        _ => return, // examples dir not present in this checkout layout
    };
    for entry in std::fs::read_dir(&dir).expect("read examples") {
        let path = entry.unwrap().path();
        if path.extension().map(|e| e == "k2").unwrap_or(false) {
            let src = std::fs::read_to_string(&path).unwrap();
            // A parse error is allowed to be reported as Err; anything that parses
            // must yield balanced HTML.
            if let Ok(model) = build_doc_model(&src, &path.to_string_lossy()) {
                for p in render::emit_html(&model) {
                    render::assert_balanced(&p.contents)
                        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                }
                let _ = render::emit_markdown(&model);
            }
        }
    }
}
