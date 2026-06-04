//! Tests for the document store: open/close, full and incremental sync.

use super::*;
use crate::json::JsonValue;

/// Builds a full-replacement change entry.
fn full(text: &str) -> JsonValue {
    JsonValue::obj(vec![("text", JsonValue::str(text))])
}

/// Builds an incremental change entry over a (line,char)..(line,char) range.
fn incr(sl: i64, sc: i64, el: i64, ec: i64, text: &str) -> JsonValue {
    let pos = |l: i64, c: i64| {
        JsonValue::obj(vec![
            ("line", JsonValue::num(l)),
            ("character", JsonValue::num(c)),
        ])
    };
    JsonValue::obj(vec![
        (
            "range",
            JsonValue::obj(vec![("start", pos(sl, sc)), ("end", pos(el, ec))]),
        ),
        ("text", JsonValue::str(text)),
    ])
}

#[test]
fn open_and_get() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "const x = 1;\n".to_string());
    assert_eq!(store.get("u").unwrap().text, "const x = 1;\n");
    assert_eq!(store.get("u").unwrap().version, 1);
}

#[test]
fn full_replacement() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "old".to_string());
    store.apply_changes("u", 2, &[full("brand new")]);
    assert_eq!(store.get("u").unwrap().text, "brand new");
    assert_eq!(store.get("u").unwrap().version, 2);
}

#[test]
fn incremental_edit_replaces_range() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "const x = 1;\n".to_string());
    // Replace the `1` (line 0, chars 10..11) with `42`.
    store.apply_changes("u", 2, &[incr(0, 10, 0, 11, "42")]);
    assert_eq!(store.get("u").unwrap().text, "const x = 42;\n");
}

#[test]
fn incremental_insert_at_point() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "ab".to_string());
    // Insert "X" between a and b (zero-width range at char 1).
    store.apply_changes("u", 2, &[incr(0, 1, 0, 1, "X")]);
    assert_eq!(store.get("u").unwrap().text, "aXb");
}

#[test]
fn incremental_across_lines() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "line1\nline2\n".to_string());
    // Delete from (0,4) to (1,1): "1\nl".
    store.apply_changes("u", 2, &[incr(0, 4, 1, 1, "")]);
    assert_eq!(store.get("u").unwrap().text, "lineine2\n");
}

#[test]
fn multibyte_incremental_edit() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "é=𝄞;".to_string());
    // Replace `𝄞` (UTF-16 chars 2..4 on line 0) with `7`.
    store.apply_changes("u", 2, &[incr(0, 2, 0, 4, "7")]);
    assert_eq!(store.get("u").unwrap().text, "é=7;");
}

#[test]
fn close_removes() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "x".to_string());
    store.close("u");
    assert!(store.get("u").is_none());
}

#[test]
fn analysis_is_cached_and_recomputed() {
    let mut store = DocumentStore::new();
    store.open("u".to_string(), 1, "const x = 1;\n".to_string());
    let doc = store.get_mut("u").unwrap();
    // First access computes; second returns the same source.
    assert_eq!(doc.analysis().source, "const x = 1;\n");
    store.apply_changes("u", 2, &[full("const y = 2;\n")]);
    let doc = store.get_mut("u").unwrap();
    assert_eq!(doc.analysis().source, "const y = 2;\n");
}
