//! LSP base-protocol framing: `Content-Length`-delimited JSON-RPC over a byte
//! stream.
//!
//! Each message is an HTTP-like header block followed by a JSON body:
//!
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <N bytes of UTF-8 JSON>
//! ```
//!
//! The reader parses the headers (only `Content-Length` is significant; any
//! other header, e.g. `Content-Type`, is ignored), reads exactly `N` bytes, and
//! decodes the JSON. The writer serializes a value, measures its **byte** length,
//! and emits the framed message. Both never panic: a malformed frame becomes an
//! `io::Error` (or `Ok(None)` at a clean EOF) that the dispatch loop handles.
//!
//! ## Bounded body allocation
//!
//! A `Content-Length` is attacker-controlled: a single malformed (or merely
//! corrupted) header line can carry an enormous value. The reader therefore
//! **never** allocates `len` bytes up front from an unvalidated header. It
//! rejects anything over [`MAX_BODY`] with a clean `InvalidData` error, and even
//! for an accepted length it reserves the buffer fallibly with
//! [`Vec::try_reserve_exact`] and fills it incrementally via `take(len)`, so a
//! lying length (one larger than the bytes that actually arrive) is a per-message
//! `UnexpectedEof`, never a Rust allocation abort or a `capacity overflow` panic.
//! This upholds the crate's never-abort contract (`lib.rs`) against a hostile or
//! buggy client.

use std::io::{self, BufRead, Read, Write};

use crate::json::{parse_json, to_json_string, JsonValue};

/// The largest JSON body the reader will accept, in bytes. LSP messages are
/// tiny (a big `didChange` is a few hundred KiB at most), so 64 MiB is already
/// far more than any real client sends; anything larger is treated as a
/// malformed/hostile frame rather than honored with a giant allocation.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Reads one framed JSON-RPC message from `reader`.
///
/// Returns `Ok(Some(value))` for a message, `Ok(None)` at a clean EOF before any
/// header (an orderly stdin close), or `Err` for an I/O error or a malformed
/// frame. The JSON body is parsed with the pure-`std` parser; a body that is not
/// valid JSON is surfaced as an `InvalidData` error rather than a panic.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<JsonValue>> {
    let mut content_length: Option<usize> = None;
    let mut saw_any_header = false;

    // ---- header block ----------------------------------------------------
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF. Clean only if we have not started a message.
            if saw_any_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF in the middle of a message header",
                ));
            }
            return Ok(None);
        }
        // A blank line (just the CRLF) ends the header block.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        saw_any_header = true;
        if let Some((key, value)) = trimmed.split_once(':') {
            if key.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
            // Any other header (Content-Type, …) is accepted and ignored.
        }
    }

    let len = match content_length {
        Some(len) => len,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing or invalid Content-Length header",
            ));
        }
    };

    // Reject an absurd length *before* touching the allocator. A header is
    // attacker-controlled, so a 99999999999 / u64::MAX value must be a clean
    // per-message error, not an uncatchable allocation abort or a capacity
    // overflow panic.
    if len > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Content-Length too large",
        ));
    }

    // ---- body ------------------------------------------------------------
    // Reserve fallibly and fill incrementally: `take(len)` caps the read at the
    // promised length, and `read_to_end` grows the buffer only as bytes actually
    // arrive, so a lying length (more than the stream delivers) surfaces as a
    // short read we reject below — never a giant up-front allocation.
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Content-Length too large"))?;
    let read = reader.take(len as u64).read_to_end(&mut buf)?;
    if read != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "EOF before the full message body",
        ));
    }
    let text = String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message body is not UTF-8"))?;
    let value =
        parse_json(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(value))
}

/// Writes one framed JSON-RPC message to `writer` and flushes.
///
/// The `Content-Length` is the length of the UTF-8 body in **bytes**, not chars.
pub fn write_message(writer: &mut impl Write, value: &JsonValue) -> io::Result<()> {
    let body = to_json_string(value);
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    /// Reads one message from a literal frame string.
    fn read_one(frame: &str) -> io::Result<Option<JsonValue>> {
        let mut reader = BufReader::new(Cursor::new(frame.as_bytes().to_vec()));
        read_message(&mut reader)
    }

    #[test]
    fn well_formed_message_round_trips() {
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let value = read_one(&frame).unwrap().unwrap();
        assert_eq!(value.get("id").and_then(|i| i.as_i64()), Some(1));
    }

    #[test]
    fn oversized_content_length_is_a_clean_error_not_an_abort() {
        // The blocker repro: a 99999999999-byte body would, if allocated up
        // front, abort the process with SIGABRT. It must be a clean `Err`.
        let frame = "Content-Length: 99999999999\r\n\r\n{}";
        let err = read_one(frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn u64_max_content_length_is_a_clean_error_not_a_panic() {
        // The second repro: u64::MAX previously caused a `capacity overflow`
        // panic. It must be a clean `Err` (rejected by the MAX_BODY cap).
        let frame = "Content-Length: 18446744073709551615\r\n\r\n{}";
        let err = read_one(frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn lying_length_within_cap_is_unexpected_eof_not_a_giant_alloc() {
        // A length the stream cannot satisfy: accepted by the cap, but the body
        // is short, so it is a clean EOF rather than honoring the claim.
        let frame = "Content-Length: 1000000\r\n\r\n{}";
        let err = read_one(frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn clean_eof_before_any_header_is_ok_none() {
        assert_eq!(read_one("").unwrap(), None);
    }
}
