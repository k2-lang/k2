//! Tests for the pure-`std` JSON parser/serializer: round-trips, escapes,
//! surrogate pairs, numbers, order preservation, and malformed-input rejection.

use super::*;

/// Parses `src`, asserting it succeeds.
fn p(src: &str) -> JsonValue {
    parse_json(src).unwrap_or_else(|e| panic!("parse of {src:?} failed: {e}"))
}

#[test]
fn round_trips_primitives() {
    assert_eq!(to_json_string(&p("null")), "null");
    assert_eq!(to_json_string(&p("true")), "true");
    assert_eq!(to_json_string(&p("false")), "false");
    assert_eq!(to_json_string(&p("0")), "0");
    assert_eq!(to_json_string(&p("-42")), "-42");
    assert_eq!(to_json_string(&p("\"hi\"")), "\"hi\"");
}

#[test]
fn integers_have_no_decimal_point() {
    // `id` round-trips as a bare integer, never `2.0`.
    assert_eq!(to_json_string(&p("2")), "2");
    assert_eq!(to_json_string(&JsonValue::num(58)), "58");
    assert_eq!(to_json_string(&JsonValue::Int(100)), "100");
    // A fractional `f64` that happens to be integral still drops the `.0`.
    assert_eq!(to_json_string(&JsonValue::Num(100.0)), "100");
}

#[test]
fn integral_numbers_parse_as_lossless_int() {
    // Plain integers become `Int`, not `Num`, so precision is never lost.
    assert_eq!(p("0"), JsonValue::Int(0));
    assert_eq!(p("-7"), JsonValue::Int(-7));
    assert_eq!(p("123"), JsonValue::Int(123));
}

#[test]
fn large_integer_id_round_trips_exactly() {
    // 2^53 + 1: an `f64` would round this to 2^53, breaking JSON-RPC id matching.
    // The lossless `Int` variant preserves it bit-for-bit through a round-trip.
    let big = "9007199254740993";
    let v = p(big);
    assert_eq!(v, JsonValue::Int(9_007_199_254_740_993));
    assert_eq!(v.as_i64(), Some(9_007_199_254_740_993));
    assert_eq!(to_json_string(&v), big);
    // The full request envelope echoes the id verbatim.
    let req = p(r#"{"jsonrpc":"2.0","id":9007199254740993}"#);
    assert_eq!(to_json_string(req.get("id").unwrap()), big);
}

#[test]
fn parses_number_forms() {
    assert_eq!(p("0").as_i64(), Some(0));
    assert_eq!(p("-7").as_i64(), Some(-7));
    assert_eq!(p("123").as_i64(), Some(123));
    // Fraction and exponent parse (used only defensively by the protocol).
    assert_eq!(p("3.5"), JsonValue::Num(3.5));
    assert_eq!(p("1e3"), JsonValue::Num(1000.0));
    assert_eq!(p("1.5e-2"), JsonValue::Num(0.015));
    assert_eq!(p("-0.25"), JsonValue::Num(-0.25));
}

#[test]
fn round_trips_objects_and_arrays() {
    let src = "{\"a\":1,\"b\":[true,null,\"x\"],\"c\":{\"d\":-3}}";
    assert_eq!(to_json_string(&p(src)), src);
}

#[test]
fn object_key_order_is_preserved() {
    // Insertion order is the serialization order — load-bearing for goldens.
    let src = "{\"z\":1,\"a\":2,\"m\":3}";
    assert_eq!(to_json_string(&p(src)), src);
}

#[test]
fn decodes_string_escapes() {
    let v = p("\"a\\nb\\tc\\\"d\\\\e\\/f\"");
    assert_eq!(v.as_str(), Some("a\nb\tc\"d\\e/f"));
    // \b and \f decode to the right control characters.
    let v = p("\"\\b\\f\"");
    assert_eq!(v.as_str(), Some("\u{0008}\u{000C}"));
}

#[test]
fn decodes_bmp_unicode_escape() {
    let v = p("\"\\u00e9\""); // é
    assert_eq!(v.as_str(), Some("é"));
    let v = p("\"\\u03bb\""); // λ
    assert_eq!(v.as_str(), Some("λ"));
}

#[test]
fn decodes_surrogate_pair() {
    // U+1D11E MUSICAL SYMBOL G CLEF, encoded as a UTF-16 surrogate pair.
    let v = p("\"\\uD834\\uDD1E\"");
    assert_eq!(v.as_str(), Some("𝄞"));
}

#[test]
fn serializes_control_chars_as_escapes() {
    let v = JsonValue::str("tab\there\nnewline\u{0001}ctrl");
    let out = to_json_string(&v);
    assert!(out.contains("\\t"));
    assert!(out.contains("\\n"));
    assert!(out.contains("\\u0001"));
    // Round-trips back to the original.
    assert_eq!(p(&out), v);
}

#[test]
fn emits_non_ascii_verbatim() {
    let v = JsonValue::str("héllo 𝄞");
    let out = to_json_string(&v);
    assert_eq!(out, "\"héllo 𝄞\"");
    assert_eq!(p(&out), v);
}

#[test]
fn empty_object_and_array() {
    assert_eq!(to_json_string(&p("{}")), "{}");
    assert_eq!(to_json_string(&p("[]")), "[]");
    assert_eq!(to_json_string(&p("{ }")), "{}");
    assert_eq!(to_json_string(&p("[ ]")), "[]");
}

#[test]
fn skips_insignificant_whitespace() {
    let v = p("  {\n  \"k\" : [ 1 , 2 ]\n}  ");
    assert_eq!(to_json_string(&v), "{\"k\":[1,2]}");
}

#[test]
fn malformed_inputs_error_not_panic() {
    // Each of these must return Err, never panic.
    for bad in [
        "",
        "{",
        "[",
        "\"unterminated",
        "{\"k\":}",
        "{\"k\" 1}",
        "[1,]",
        "{\"a\":1,}",
        "tru",
        "nul",
        "01",
        "1.",
        "1e",
        "\"\\q\"",     // invalid escape
        "\"\\uD834\"", // lone high surrogate
        "\"\\uDD1E\"", // lone low surrogate
        "123 456",     // trailing content
    ] {
        assert!(parse_json(bad).is_err(), "expected error for {bad:?}");
    }
}

#[test]
fn deep_nesting_is_rejected_not_overflow() {
    // Build a document deeper than MAX_DEPTH; it must error, not crash.
    let depth = (MAX_DEPTH as usize) + 50;
    let src = "[".repeat(depth) + &"]".repeat(depth);
    assert!(parse_json(&src).is_err());
}

#[test]
fn deep_within_cap_succeeds() {
    let depth = (MAX_DEPTH as usize) - 2;
    let src = "[".repeat(depth) + "1" + &"]".repeat(depth);
    assert!(parse_json(&src).is_ok());
}

#[test]
fn parses_a_real_initialize_blob() {
    let src = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":4242,"capabilities":{},"rootUri":null}}"#;
    let v = p(src);
    assert_eq!(v.get("jsonrpc").and_then(|x| x.as_str()), Some("2.0"));
    assert_eq!(v.get("id").and_then(|x| x.as_i64()), Some(1));
    assert_eq!(v.get("method").and_then(|x| x.as_str()), Some("initialize"));
    let params = v.get("params").unwrap();
    assert_eq!(params.get("processId").and_then(|x| x.as_i64()), Some(4242));
    assert_eq!(params.get("rootUri"), Some(&JsonValue::Null));
}

#[test]
fn id_can_be_string_or_number_and_echoes_verbatim() {
    // A string id is preserved as a Str; a number id as a Num. We never coerce.
    let s = p("{\"id\":\"abc\"}");
    assert_eq!(s.get("id"), Some(&JsonValue::Str("abc".to_string())));
    let n = p("{\"id\":7}");
    assert_eq!(n.get("id"), Some(&JsonValue::Int(7)));
}
