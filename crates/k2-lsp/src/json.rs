//! A minimal, dependency-free JSON value, parser, and serializer.
//!
//! The Language Server Protocol's base layer is JSON-RPC 2.0, so the server
//! needs to read and write JSON. Per the toolchain's pure-`std` rule there is no
//! `serde`/`serde_json`: this module hand-rolls exactly the subset the protocol
//! uses — objects, arrays, strings (with `\`-escapes and `\u` Unicode including
//! surrogate pairs), numbers, booleans, and `null` — and round-trips them
//! losslessly.
//!
//! ## Design choices
//!
//! * **[`JsonValue::Object`] is a `Vec<(String, JsonValue)>`, not a `HashMap`.**
//!   Insertion order is preserved, which makes serialization deterministic (so
//!   golden tests can assert exact bytes) and avoids pulling in a hasher for the
//!   tiny objects LSP exchanges. Lookup is a linear scan, which is fine at these
//!   sizes.
//! * **Integral numbers are stored losslessly as `i64`.** Every number the
//!   protocol carries that matters (request ids, line/character, lengths,
//!   severities) is an integer, so [`JsonValue::Int`] holds it exactly — even a
//!   request id past `2^53` such as `9007199254740993`, which an `f64` would
//!   silently round. A genuine *fractional* number (the protocol never sends one,
//!   but a client might) falls back to [`JsonValue::Num`] (`f64`). Integers
//!   serialize without a decimal point (`2`, never `2.0`).
//! * **A request id may be a number *or* a string.** We never reinterpret it: the
//!   raw [`JsonValue`] is echoed back verbatim in the response — an integer id
//!   round-trips bit-for-bit — sidestepping the int-vs-string ambiguity entirely.
//!   A non-finite (`NaN`/`Infinity`) number cannot occur (it is not valid JSON, so
//!   the parser rejects it), so an id never silently becomes `null`.
//! * **Never panics.** Malformed input returns a [`JsonError`]; a depth guard
//!   stops an adversarially deep document from overflowing the native stack.

use std::fmt;

/// A parsed JSON value.
///
/// This is deliberately small: the variants are the JSON value kinds, with the
/// number kind split into a lossless integral [`JsonValue::Int`] (`i64`) and a
/// fractional [`JsonValue::Num`] (`f64`). [`JsonValue::Object`] is an ordered
/// association list (see the module docs for why).
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    /// JSON `null`.
    Null,
    /// JSON `true` / `false`.
    Bool(bool),
    /// A JSON number with no fractional part, stored exactly as an `i64`. Request
    /// ids, positions, lengths, and severities all land here, so they round-trip
    /// without `f64` precision loss.
    Int(i64),
    /// A JSON number with a fractional part (or one outside `i64` range), stored
    /// as an `f64`. The protocol never sends one; kept for completeness.
    Num(f64),
    /// A JSON string, decoded to a Rust `String` (UTF-8).
    Str(String),
    /// A JSON array.
    Array(Vec<JsonValue>),
    /// A JSON object, as an insertion-ordered list of key/value pairs.
    Object(Vec<(String, JsonValue)>),
}

/// A JSON parse error: a human-readable message plus the byte offset into the
/// input where the problem was detected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    /// What went wrong.
    pub message: String,
    /// The byte offset into the source where the error was detected.
    pub offset: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

impl JsonValue {
    // ---- accessors ------------------------------------------------------

    /// Looks up `key` in an object value, returning the associated value or
    /// `None` (also `None` if `self` is not an object). Linear scan; objects are
    /// small.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string contents, if this is a [`JsonValue::Str`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// The number as `i64`, if this is a numeric value. An [`JsonValue::Int`] is
    /// returned exactly; a fractional [`JsonValue::Num`] is truncated toward zero.
    /// Used for ids, line/character, lengths, and severities, all integers.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(n) => Some(*n),
            JsonValue::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    /// The number as a `u32` (clamping negatives to `0`), if a number. LSP
    /// positions/lengths are non-negative integers.
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            JsonValue::Int(n) if *n >= 0 => Some((*n).min(i64::from(u32::MAX)) as u32),
            JsonValue::Int(_) => Some(0),
            JsonValue::Num(n) if *n >= 0.0 => Some(*n as u32),
            JsonValue::Num(_) => Some(0),
            _ => None,
        }
    }

    /// The array contents, if this is a [`JsonValue::Array`].
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// The boolean contents, if this is a [`JsonValue::Bool`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    // ---- builders -------------------------------------------------------

    /// Builds an object from a list of `(&str, value)` pairs, preserving order.
    pub fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
        JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Builds an array from a list of values.
    pub fn arr(items: Vec<JsonValue>) -> JsonValue {
        JsonValue::Array(items)
    }

    /// Builds a string value.
    pub fn str(s: impl Into<String>) -> JsonValue {
        JsonValue::Str(s.into())
    }

    /// Builds an integral number value from an `i64`, stored losslessly.
    pub fn num(n: i64) -> JsonValue {
        JsonValue::Int(n)
    }
}

// =========================================================================
//  Parser
// =========================================================================

/// A generous cap on JSON nesting depth. The protocol never approaches this; the
/// guard exists purely so a pathologically nested document cannot overflow the
/// Rust call stack (a stack overflow is an uncatchable abort, which would break
/// the server's never-panic contract).
const MAX_DEPTH: u32 = 128;

/// Parses a complete JSON document, returning the value or a [`JsonError`].
///
/// Trailing whitespace is allowed; any other trailing content is an error.
pub fn parse_json(src: &str) -> Result<JsonValue, JsonError> {
    let mut p = JsonParser {
        bytes: src.as_bytes(),
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after JSON value"));
    }
    Ok(value)
}

/// The mutable state of one recursive-descent JSON parse over raw UTF-8 bytes.
struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: u32,
}

impl JsonParser<'_> {
    /// Builds an error anchored at the current position.
    fn err(&self, message: impl Into<String>) -> JsonError {
        JsonError {
            message: message.into(),
            offset: self.pos,
        }
    }

    /// The byte at the cursor, or `None` at end of input.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Skips JSON insignificant whitespace (` \t\r\n`).
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Parses any JSON value at the cursor (whitespace already skipped).
    fn parse_value(&mut self) -> Result<JsonValue, JsonError> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(JsonValue::Str(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.parse_number(),
            Some(_) => Err(self.err("unexpected character at start of value")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    /// Enters one level of nesting, failing if the depth cap is exceeded.
    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        Ok(())
    }

    /// Leaves one level of nesting.
    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Parses an object `{ "k": v, ... }`.
    fn parse_object(&mut self) -> Result<JsonValue, JsonError> {
        self.enter()?;
        self.pos += 1; // consume '{'
        let mut pairs: Vec<(String, JsonValue)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.leave();
            return Ok(JsonValue::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in object"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1; // consume ':'
            self.skip_ws();
            let value = self.parse_value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.leave();
                    return Ok(JsonValue::Object(pairs));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    /// Parses an array `[ v, ... ]`.
    fn parse_array(&mut self) -> Result<JsonValue, JsonError> {
        self.enter()?;
        self.pos += 1; // consume '['
        let mut items: Vec<JsonValue> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.leave();
            return Ok(JsonValue::Array(items));
        }
        loop {
            self.skip_ws();
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    self.leave();
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    /// Parses a string literal at the cursor, decoding all escapes (including
    /// `\uXXXX` and surrogate pairs) into a Rust `String`.
    fn parse_string(&mut self) -> Result<String, JsonError> {
        // The cursor is on the opening '"'.
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(self.err("unterminated string")),
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                // A raw control character is illegal in a JSON string.
                0x00..=0x1F => return Err(self.err("unescaped control character in string")),
                _ => {
                    // Decode the next UTF-8 scalar from the byte stream.
                    let ch = self.next_utf8_char()?;
                    out.push(ch);
                }
            }
        }
    }

    /// Decodes one escape sequence (the `\` already consumed) into `out`.
    fn parse_escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let b = match self.peek() {
            Some(b) => b,
            None => return Err(self.err("unterminated escape")),
        };
        self.pos += 1;
        match b {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let cp = self.parse_hex4()?;
                if (0xD800..=0xDBFF).contains(&cp) {
                    // A high surrogate must be followed by a `\uXXXX` low
                    // surrogate to form one astral scalar.
                    if self.peek() != Some(b'\\') {
                        return Err(self.err("expected low surrogate after high surrogate"));
                    }
                    self.pos += 1;
                    if self.peek() != Some(b'u') {
                        return Err(self.err("expected \\u low surrogate"));
                    }
                    self.pos += 1;
                    let lo = self.parse_hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(self.err("invalid low surrogate"));
                    }
                    let c = 0x1_0000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    match char::from_u32(c) {
                        Some(ch) => out.push(ch),
                        None => return Err(self.err("invalid surrogate pair")),
                    }
                } else if (0xDC00..=0xDFFF).contains(&cp) {
                    return Err(self.err("unexpected lone low surrogate"));
                } else {
                    match char::from_u32(cp) {
                        Some(ch) => out.push(ch),
                        None => return Err(self.err("invalid unicode escape")),
                    }
                }
            }
            _ => return Err(self.err("invalid escape character")),
        }
        Ok(())
    }

    /// Parses exactly four hex digits into a `u32` code point.
    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(self.err("unterminated \\u escape")),
            };
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err("invalid hex digit in \\u escape")),
            };
            value = (value << 4) | u32::from(digit);
            self.pos += 1;
        }
        Ok(value)
    }

    /// Decodes the next UTF-8 scalar from the byte stream, advancing the cursor.
    /// Invalid UTF-8 is a parse error rather than a panic.
    fn next_utf8_char(&mut self) -> Result<char, JsonError> {
        let rest = &self.bytes[self.pos..];
        // `str::from_utf8` on a small lookahead is the simplest correct decode.
        let max = rest.len().min(4);
        for take in 1..=max {
            if let Ok(s) = std::str::from_utf8(&rest[..take]) {
                if let Some(ch) = s.chars().next() {
                    self.pos += take;
                    return Ok(ch);
                }
            }
        }
        Err(self.err("invalid UTF-8 in string"))
    }

    /// Parses a `true`/`false` literal.
    fn parse_bool(&mut self) -> Result<JsonValue, JsonError> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    /// Parses a `null` literal.
    fn parse_null(&mut self) -> Result<JsonValue, JsonError> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    /// Parses a JSON number (`-?int frac? exp?`) via the standard grammar. An
    /// integer with no fraction or exponent that fits in `i64` is stored exactly
    /// as a [`JsonValue::Int`] (so large ids survive the round-trip); anything
    /// fractional, exponential, or out of `i64` range falls back to `f64`.
    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        // Fraction.
        let mut is_integral = true;
        if self.peek() == Some(b'.') {
            is_integral = false;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit after decimal point"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Exponent.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_integral = false;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // The slice is ASCII by construction, so `from_utf8` cannot fail.
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number encoding"))?;
        // Prefer the lossless integer form for a plain integer literal.
        if is_integral {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(JsonValue::Int(i));
            }
        }
        text.parse::<f64>()
            .map(JsonValue::Num)
            .map_err(|_| self.err("number out of range"))
    }
}

// =========================================================================
//  Serializer
// =========================================================================

/// Serializes a [`JsonValue`] to its compact JSON text (no insignificant
/// whitespace), deterministically by object insertion order.
pub fn to_json_string(value: &JsonValue) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// Appends the JSON text of `value` to `out`.
fn write_value(out: &mut String, value: &JsonValue) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Int(n) => out.push_str(&n.to_string()),
        JsonValue::Num(n) => write_number(out, *n),
        JsonValue::Str(s) => write_string(out, s),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        JsonValue::Object(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, v);
            }
            out.push('}');
        }
    }
}

/// Writes a fractional [`JsonValue::Num`]. Integral values are carried by
/// [`JsonValue::Int`] and serialized directly, so this handles only genuine
/// fractionals (and, defensively, the impossible non-finite case): an integral
/// `f64` still prints without a trailing `.0`, and a `NaN`/`Inf` — which the
/// parser can never produce, since they are not valid JSON — degrades to `null`.
fn write_number(out: &mut String, n: f64) {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        // Exactly representable integer: print as `i64`.
        out.push_str(&(n as i64).to_string());
    } else if n.is_finite() {
        out.push_str(&n.to_string());
    } else {
        out.push_str("null");
    }
}

/// Writes a JSON string literal, escaping `"`, `\`, and control characters. The
/// body is valid UTF-8, so non-ASCII scalars are emitted verbatim (also valid
/// JSON) rather than as `\u` escapes.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests;
