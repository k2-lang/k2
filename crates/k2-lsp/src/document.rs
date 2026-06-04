//! The in-memory document store: every open buffer, its version, and its cached
//! analysis.
//!
//! URIs are treated as opaque strings — the in-memory model needs no
//! `file://` → path decoding. Each [`Document`] caches its [`Analysis`], which is
//! invalidated (set to `None`) on every content change and recomputed lazily.

use std::collections::HashMap;

use crate::analysis::Analysis;
use crate::json::JsonValue;
use crate::position::PositionMap;

/// One open document.
pub struct Document {
    /// The document URI (the store key), kept for convenience.
    pub uri: String,
    /// The client-reported version, echoed back in `publishDiagnostics`.
    pub version: i64,
    /// The current full text of the buffer.
    pub text: String,
    /// The cached analysis, or `None` if it must be recomputed.
    analysis: Option<Analysis>,
}

impl Document {
    /// The analysis for this document, computing and caching it if necessary.
    pub fn analysis(&mut self) -> &Analysis {
        if self.analysis.is_none() {
            self.analysis = Some(Analysis::compute(self.text.clone()));
        }
        // Just populated above, so the unwrap is infallible.
        self.analysis.as_ref().expect("analysis just computed")
    }

    /// Invalidates the cached analysis (called on every edit).
    fn invalidate(&mut self) {
        self.analysis = None;
    }
}

/// The collection of open documents, keyed by URI.
#[derive(Default)]
pub struct DocumentStore {
    docs: HashMap<String, Document>,
}

impl DocumentStore {
    /// Builds an empty store.
    pub fn new() -> DocumentStore {
        DocumentStore::default()
    }

    /// Opens (or replaces) a document with full text.
    pub fn open(&mut self, uri: String, version: i64, text: String) {
        self.docs.insert(
            uri.clone(),
            Document {
                uri,
                version,
                text,
                analysis: None,
            },
        );
    }

    /// Applies a `didChange` batch — either full-document replacements
    /// (`change.text` with no `range`) or incremental edits (`range` + `text`) —
    /// in order, then invalidates the cached analysis.
    ///
    /// Unknown or malformed change entries are skipped rather than panicking.
    pub fn apply_changes(&mut self, uri: &str, version: i64, changes: &[JsonValue]) {
        let doc = match self.docs.get_mut(uri) {
            Some(doc) => doc,
            None => return,
        };
        doc.version = version;
        for change in changes {
            match change.get("range") {
                // Full replacement.
                None => {
                    if let Some(text) = change.get("text").and_then(|t| t.as_str()) {
                        doc.text = text.to_string();
                    }
                }
                // Incremental edit over a range.
                Some(range) => {
                    let text = change.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    apply_incremental(&mut doc.text, range, text);
                }
            }
        }
        doc.invalidate();
    }

    /// Closes a document, dropping its text and analysis.
    pub fn close(&mut self, uri: &str) {
        self.docs.remove(uri);
    }

    /// A document by URI, if open.
    pub fn get(&self, uri: &str) -> Option<&Document> {
        self.docs.get(uri)
    }

    /// A mutable document by URI, if open (so callers can compute its analysis).
    pub fn get_mut(&mut self, uri: &str) -> Option<&mut Document> {
        self.docs.get_mut(uri)
    }
}

/// Applies one incremental change: replaces the half-open scalar range described
/// by the LSP `range` with `text`. The range is converted through a *fresh*
/// position map over the current text (correct because each change is applied
/// before the next is converted).
fn apply_incremental(doc_text: &mut String, range: &JsonValue, text: &str) {
    let pm = PositionMap::new(doc_text);
    let start = match range.get("start") {
        Some(p) => pm.position_to_offset(
            p.get("line").and_then(|x| x.as_u32()).unwrap_or(0),
            p.get("character").and_then(|x| x.as_u32()).unwrap_or(0),
        ),
        None => return,
    };
    let end = match range.get("end") {
        Some(p) => pm.position_to_offset(
            p.get("line").and_then(|x| x.as_u32()).unwrap_or(0),
            p.get("character").and_then(|x| x.as_u32()).unwrap_or(0),
        ),
        None => return,
    };
    // Convert the scalar offsets to byte indices for the splice.
    let chars: Vec<char> = doc_text.chars().collect();
    let lo = start.min(end) as usize;
    let hi = start.max(end).min(chars.len() as u32) as usize;
    let byte_lo: usize = chars[..lo].iter().map(|c| c.len_utf8()).sum();
    let byte_hi: usize = byte_lo + chars[lo..hi].iter().map(|c| c.len_utf8()).sum::<usize>();
    doc_text.replace_range(byte_lo..byte_hi, text);
}

#[cfg(test)]
mod tests;
