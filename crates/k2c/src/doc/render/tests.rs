//! Unit tests for the documentation renderer: escaping, the Markdown→HTML subset,
//! cross-links, slugs, doc-comment normalization, and a tag-balance invariant.

use super::*;
use std::collections::HashMap;

/// No-cross-link map.
fn no_links() -> HashMap<String, String> {
    HashMap::new()
}

#[test]
fn escapes_all_html_metacharacters() {
    let s = esc_html("<script>&\"'");
    assert_eq!(s, "&lt;script&gt;&amp;&quot;&#39;");
    assert!(!s.contains('<'));
    assert!(!s.contains("script>"));
}

#[test]
fn doc_to_markdown_strips_prefix_and_one_space() {
    // `///` then one optional space is stripped; inner indentation is kept.
    assert_eq!(doc_to_markdown("/// A\n///  B\n///"), "A\n B\n");
    // A line with no `///` (already stripped) passes through.
    assert_eq!(doc_to_markdown("plain"), "plain");
}

#[test]
fn heading_code_and_link_render() {
    let html = md_to_html("# Title\n\nA `code` and [x](http://e)", &no_links());
    assert!(html.contains("<h1"), "missing h1: {html}");
    assert!(
        html.contains("<code>code</code>"),
        "missing code span: {html}"
    );
    assert!(
        html.contains("<a href=\"http://e\">x</a>"),
        "missing link: {html}"
    );
    assert_balanced(&html).expect("balanced");
}

#[test]
fn doc_text_is_escaped_no_raw_script() {
    let html = md_to_html("<script>alert(1)</script> & \"q\"", &no_links());
    assert!(!html.contains("<script>"), "raw script leaked: {html}");
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("&amp;"));
    assert_balanced(&html).expect("balanced");
}

#[test]
fn javascript_url_is_neutralized() {
    let html = md_to_html("[x](javascript:alert(1))", &no_links());
    // The unsafe scheme must NOT become an href; it renders as plain (escaped) text.
    assert!(!html.contains("href=\"javascript"), "js url leaked: {html}");
    assert_balanced(&html).expect("balanced");
}

#[test]
fn cross_link_for_known_item() {
    let mut links = HashMap::new();
    links.insert("List".to_string(), "#index-list".to_string());
    let html = md_to_html("see `List` here", &links);
    assert!(
        html.contains("<a href=\"#index-list\"><code>List</code></a>"),
        "missing cross-link: {html}"
    );
    assert_balanced(&html).expect("balanced");
}

#[test]
fn lists_render_as_ul_ol() {
    let ul = md_to_html("- one\n- two", &no_links());
    assert!(ul.contains("<ul>") && ul.contains("<li>one</li>"), "{ul}");
    assert_balanced(&ul).unwrap();
    let ol = md_to_html("1. a\n2. b", &no_links());
    assert!(ol.contains("<ol>") && ol.contains("<li>a</li>"), "{ol}");
    assert_balanced(&ol).unwrap();
}

#[test]
fn fenced_code_block_is_escaped_pre() {
    let html = md_to_html("```k2\nconst x = 1 < 2;\n```", &no_links());
    assert!(html.contains("<pre><code"), "missing pre/code: {html}");
    assert!(html.contains("1 &lt; 2"), "code not escaped: {html}");
    assert_balanced(&html).unwrap();
}

#[test]
fn unterminated_fence_does_not_panic_and_is_balanced() {
    let html = md_to_html("```k2\nno closing fence here\nmore", &no_links());
    assert!(html.contains("<pre>"), "{html}");
    assert_balanced(&html).expect("balanced even with unterminated fence");
}

#[test]
fn empty_and_lone_hash_are_total() {
    assert_balanced(&md_to_html("", &no_links())).unwrap();
    assert_balanced(&md_to_html("#", &no_links())).unwrap();
    assert_balanced(&md_to_html("####### too many", &no_links())).unwrap();
}

#[test]
fn bold_and_italic() {
    let html = md_to_html("**b** and *i*", &no_links());
    assert!(html.contains("<strong>b</strong>"), "{html}");
    assert!(html.contains("<em>i</em>"), "{html}");
    assert_balanced(&html).unwrap();
}

#[test]
fn slug_is_stable_and_nonempty() {
    assert_eq!(slug("Hello, World!"), "hello-world");
    assert_eq!(slug("List.init"), "list-init");
    assert_eq!(slug("!!!"), "_");
    assert_eq!(slug(""), "_");
}

#[test]
fn md_cell_escape_protects_table_rows() {
    // A literal `|` in a cell would split the GFM row into extra columns; it must be
    // escaped. Newlines collapse to spaces (a cell is single-line).
    assert_eq!(md_cell_escape("a | b"), "a \\| b");
    assert_eq!(md_cell_escape("code|span"), "code\\|span");
    assert_eq!(md_cell_escape("line1\nline2"), "line1 line2");
    assert_eq!(md_cell_escape("plain"), "plain");
}

#[test]
fn balance_checker_rejects_mismatch() {
    assert!(assert_balanced("<p>x</p>").is_ok());
    assert!(assert_balanced("<p>x").is_err());
    assert!(assert_balanced("<p>x</div>").is_err());
}
