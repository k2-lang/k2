//! Bidirectional position mapping between LSP positions and compiler offsets.
//!
//! The two coordinate systems do not agree, and getting the conversion exactly
//! right is the load-bearing correctness piece of the server:
//!
//! * **LSP** uses `Position { line, character }`, both **0-based**, where
//!   `character` counts **UTF-16 code units** (so an astral scalar like `𝄞`
//!   counts as 2).
//! * **The k2 compiler** keys every [`Span`](k2_syntax::Span) by **scalar
//!   offsets** — indices into `src.chars()` — and records 1-based `line`/`col`
//!   *only for human-readable messages*. Crucially the lexer/parser build spans
//!   over `src.chars()` (see `k2_lexer::Lexer` and `k2_parse::build_line_starts`),
//!   so `span.start`/`span.end` are **char counts, not byte offsets**.
//!
//! This module therefore converts purely between LSP UTF-16 positions and
//! **scalar (char) offsets**, which is what the compiler's spans use. We never
//! trust `span.line`/`col` for ranges — they are a scalar column count, not
//! UTF-16 — and recompute every LSP [`Range`] from the scalar `start`/`end`.

use crate::json::JsonValue;
use k2_syntax::Span;

/// A precomputed line index over one document, enabling O(log n) offset→position
/// and O(line length) position→offset conversions.
pub struct PositionMap {
    /// The document decoded to scalars, for slicing per line.
    chars: Vec<char>,
    /// `line_starts[n]` is the scalar offset of the first character of 0-based
    /// line `n`. Line 0 starts at 0; a new entry is pushed after each `'\n'`.
    line_starts: Vec<u32>,
}

impl PositionMap {
    /// Builds a position map over `src`, scanning it once.
    ///
    /// A `'\n'` terminates a line; the character after it begins the next. A
    /// lone `'\r'` (or the `'\r'` of a `\r\n` pair) is treated as ordinary
    /// content on the preceding line, matching how editors count columns.
    pub fn new(src: &str) -> PositionMap {
        let chars: Vec<char> = src.chars().collect();
        let mut line_starts = vec![0u32];
        for (i, &c) in chars.iter().enumerate() {
            if c == '\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        PositionMap { chars, line_starts }
    }

    /// The number of lines (always at least 1).
    fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The scalar offset just past the last character of 0-based `line` —
    /// i.e. the start of the next line, or the document end for the last line.
    fn line_end_offset(&self, line: usize) -> u32 {
        if line + 1 < self.line_count() {
            self.line_starts[line + 1]
        } else {
            self.chars.len() as u32
        }
    }

    /// Converts an LSP `Position { line, character }` (0-based, UTF-16) to a
    /// scalar offset into the source. Out-of-range lines/characters clamp to the
    /// nearest valid boundary, and a `character` that lands inside a surrogate
    /// pair clamps to the enclosing char boundary — so a malformed position can
    /// never panic or produce an out-of-bounds offset.
    pub fn position_to_offset(&self, line: u32, character_utf16: u32) -> u32 {
        let line = (line as usize).min(self.line_count().saturating_sub(1));
        let base = self.line_starts[line];
        // The line excludes its trailing '\n' (which begins the next line).
        let mut end = self.line_end_offset(line);
        if end > base
            && self
                .chars
                .get((end - 1) as usize)
                .map(|&c| c == '\n')
                .unwrap_or(false)
        {
            end -= 1;
        }
        // Walk the line by char, accumulating UTF-16 units until we reach the
        // requested count.
        let mut offset = base;
        let mut utf16 = 0u32;
        while offset < end && utf16 < character_utf16 {
            let c = self.chars[offset as usize];
            utf16 += c.len_utf16() as u32;
            offset += 1;
        }
        offset
    }

    /// Converts a scalar offset to an LSP `Position { line, character }`
    /// (0-based, UTF-16). Offsets past the end clamp to the document end.
    pub fn offset_to_position(&self, offset: u32) -> (u32, u32) {
        let offset = offset.min(self.chars.len() as u32);
        // Greatest `line_starts[line] <= offset` via binary search.
        let line = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };
        let base = self.line_starts[line];
        let mut utf16 = 0u32;
        for i in base..offset {
            utf16 += self.chars[i as usize].len_utf16() as u32;
        }
        (line as u32, utf16)
    }

    /// Builds the LSP `Position` JSON object for a scalar offset.
    pub fn offset_to_position_json(&self, offset: u32) -> JsonValue {
        let (line, character) = self.offset_to_position(offset);
        JsonValue::obj(vec![
            ("line", JsonValue::num(i64::from(line))),
            ("character", JsonValue::num(i64::from(character))),
        ])
    }

    /// Converts a compiler [`Span`] to an LSP `Range` JSON object, computed
    /// purely from the scalar `start`/`end` (never from `span.line`/`col`).
    pub fn span_to_range(&self, span: Span) -> JsonValue {
        JsonValue::obj(vec![
            ("start", self.offset_to_position_json(span.start)),
            ("end", self.offset_to_position_json(span.end)),
        ])
    }

    /// The LSP position of the very end of the document (last line, last UTF-16
    /// character), used as the end of a full-document replacement range.
    pub fn end_position(&self) -> (u32, u32) {
        self.offset_to_position(self.chars.len() as u32)
    }

    /// The end-of-document `Position` as JSON.
    pub fn end_position_json(&self) -> JsonValue {
        let (line, character) = self.end_position();
        JsonValue::obj(vec![
            ("line", JsonValue::num(i64::from(line))),
            ("character", JsonValue::num(i64::from(character))),
        ])
    }
}

#[cfg(test)]
mod tests;
