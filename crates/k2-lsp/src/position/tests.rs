//! Tests for the LSP <-> scalar-offset position mapping, with an emphasis on the
//! multi-byte / multi-UTF-16-unit cases a naive implementation gets wrong.

use super::*;
use k2_syntax::Span;

#[test]
fn ascii_round_trips_every_offset() {
    let src = "const x = 1;\npub fn main() {}\nlast";
    let pm = PositionMap::new(src);
    let n = src.chars().count() as u32;
    for off in 0..=n {
        let (line, ch) = pm.offset_to_position(off);
        assert_eq!(
            pm.position_to_offset(line, ch),
            off,
            "offset {off} did not round-trip (line {line}, char {ch})"
        );
    }
}

#[test]
fn ascii_positions_are_expected() {
    let src = "ab\ncde\nf";
    let pm = PositionMap::new(src);
    // 'a' at (0,0), 'b' at (0,1), '\n' boundary, 'c' at (1,0)...
    assert_eq!(pm.offset_to_position(0), (0, 0));
    assert_eq!(pm.offset_to_position(1), (0, 1));
    assert_eq!(pm.offset_to_position(3), (1, 0)); // 'c'
    assert_eq!(pm.offset_to_position(7), (2, 0)); // 'f'
    assert_eq!(pm.position_to_offset(1, 0), 3);
    assert_eq!(pm.position_to_offset(2, 0), 7);
}

#[test]
fn two_byte_scalar_counts_one_utf16_unit() {
    // `é` is one scalar, one UTF-16 unit, but two UTF-8 bytes. After it, the
    // character column is 1 (not 2 — bytes are irrelevant to LSP).
    let src = "é=1";
    let pm = PositionMap::new(src);
    // 'é' at offset 0, '=' at offset 1.
    assert_eq!(pm.offset_to_position(0), (0, 0));
    assert_eq!(pm.offset_to_position(1), (0, 1)); // after 'é'
    assert_eq!(pm.offset_to_position(2), (0, 2)); // after '='
    assert_eq!(pm.position_to_offset(0, 1), 1);
}

#[test]
fn astral_scalar_counts_two_utf16_units() {
    // `𝄞` (U+1D11E) is one scalar but TWO UTF-16 units.
    let src = "𝄞x";
    let pm = PositionMap::new(src);
    // Scalar offsets: '𝄞' at 0, 'x' at 1.
    assert_eq!(pm.offset_to_position(0), (0, 0));
    assert_eq!(pm.offset_to_position(1), (0, 2)); // after the surrogate pair
    assert_eq!(pm.offset_to_position(2), (0, 3)); // after 'x'
                                                  // Going back: character 2 lands on 'x' (offset 1).
    assert_eq!(pm.position_to_offset(0, 2), 1);
    // A character that falls *inside* the surrogate pair clamps to the char.
    assert_eq!(pm.position_to_offset(0, 1), 1);
}

#[test]
fn mixed_multibyte_line() {
    // λ (1 unit), 𝄞 (2 units), é (1 unit) then ASCII.
    let src = "λ𝄞éz";
    let pm = PositionMap::new(src);
    // offsets: λ=0, 𝄞=1, é=2, z=3
    assert_eq!(pm.offset_to_position(0), (0, 0)); // before λ
    assert_eq!(pm.offset_to_position(1), (0, 1)); // after λ
    assert_eq!(pm.offset_to_position(2), (0, 3)); // after 𝄞 (+2)
    assert_eq!(pm.offset_to_position(3), (0, 4)); // after é
    assert_eq!(pm.offset_to_position(4), (0, 5)); // after z
                                                  // And back the other way.
    assert_eq!(pm.position_to_offset(0, 0), 0);
    assert_eq!(pm.position_to_offset(0, 1), 1);
    assert_eq!(pm.position_to_offset(0, 3), 2);
    assert_eq!(pm.position_to_offset(0, 4), 3);
    assert_eq!(pm.position_to_offset(0, 5), 4);
}

#[test]
fn span_to_range_uses_scalar_offsets() {
    // A multi-byte prefix must not shift the computed range.
    let src = "é const";
    let pm = PositionMap::new(src);
    // The word `const` occupies scalar offsets 2..7.
    let span = Span::new(2, 7, 1, 3);
    let range = pm.span_to_range(span);
    let start = range.get("start").unwrap();
    let end = range.get("end").unwrap();
    assert_eq!(start.get("line").unwrap().as_i64(), Some(0));
    assert_eq!(start.get("character").unwrap().as_i64(), Some(2));
    assert_eq!(end.get("character").unwrap().as_i64(), Some(7));
}

#[test]
fn crlf_line_endings_counted() {
    // `\r\n` — the `\r` belongs to the preceding line; `\n` starts the next.
    let src = "ab\r\ncd";
    let pm = PositionMap::new(src);
    // Scalars: a=0,b=1,\r=2,\n=3,c=4,d=5
    assert_eq!(pm.offset_to_position(2), (0, 2)); // the '\r' on line 0
    assert_eq!(pm.offset_to_position(4), (1, 0)); // 'c' on line 1
                                                  // Clamping to end of line 0 must not include the '\n'.
    assert_eq!(pm.position_to_offset(0, 100), 3); // stops before '\n'
}

#[test]
fn out_of_range_inputs_clamp() {
    let src = "abc\ndef";
    let pm = PositionMap::new(src);
    // Line beyond the last clamps to the last line.
    let off = pm.position_to_offset(99, 0);
    assert_eq!(off, 4); // start of line 1 ("def")
                        // Offset past the end clamps to the document end.
    assert_eq!(pm.offset_to_position(999), pm.end_position());
}

#[test]
fn end_position_with_and_without_trailing_newline() {
    let with_nl = PositionMap::new("a\nb\n");
    // Trailing newline opens an empty line 2.
    assert_eq!(with_nl.end_position(), (2, 0));
    let without_nl = PositionMap::new("a\nbc");
    assert_eq!(without_nl.end_position(), (1, 2));
    let empty = PositionMap::new("");
    assert_eq!(empty.end_position(), (0, 0));
}

#[test]
fn char_boundary_round_trip_property() {
    // For every char boundary, position_to_offset(offset_to_position(off)) == off.
    let src = "λ x\n𝄞 = é;\nq";
    let pm = PositionMap::new(src);
    let n = src.chars().count() as u32;
    for off in 0..=n {
        let (l, c) = pm.offset_to_position(off);
        assert_eq!(pm.position_to_offset(l, c), off, "offset {off}");
    }
}
