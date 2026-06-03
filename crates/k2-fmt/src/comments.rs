//! Comment recovery and attachment.
//!
//! The parser/AST sees `///` doc comments (they live on `Item`/`Field`/
//! `SourceFile` `doc` fields) but **drops** `//` line comments entirely. A
//! lossless formatter cannot delete a single comment, so this module recovers
//! every line comment from the additive [`k2_lexer::tokenize_with_trivia`] side
//! channel and re-attaches each one, by source offset, to the AST node it
//! belongs to.
//!
//! The attachment is deterministic and offset-driven: each comment is classified
//! as **leading** (own-line, textually before a node), **trailing** (on the same
//! physical line as a node's last token), or **dangling** (own-line, inside an
//! otherwise-childless block before its `}`). The printer ([`crate::printer`])
//! then emits leading comments above a node, trailing comments after it, and
//! dangling comments inside the block. Doc comments are handled separately via
//! the AST's `doc` fields, so only line comments are tracked here.

use k2_lexer::{tokenize_with_trivia, TriviaKind};

/// One recovered `//` line comment, with the layout facts the printer needs.
#[derive(Clone, Debug)]
pub(crate) struct Comment {
    /// The verbatim comment text, including the leading `//`, with trailing
    /// whitespace trimmed (the trailing newline is already excluded by the
    /// lexer).
    pub(crate) text: String,
    /// Scalar offset of the comment's first character.
    pub(crate) start: u32,
    /// 1-based line of the comment.
    pub(crate) line: u32,
    /// `true` if only whitespace precedes the comment on its physical line (so
    /// it is a standalone / leading / dangling comment); `false` if code
    /// precedes it (a trailing same-line comment).
    pub(crate) own_line: bool,
    /// `true` if the author left at least one blank line immediately above this
    /// comment. The printer preserves at most one such blank line.
    pub(crate) blank_before: bool,
}

/// Recovers all `//` line comments from `src`, in source order, with their
/// layout facts (`own_line`, `blank_before`) computed by scanning backwards over
/// the preceding physical line. Doc comments are intentionally excluded — they
/// reach the printer through the AST's `doc` fields.
pub(crate) fn collect_comments(src: &str) -> Vec<Comment> {
    let chars: Vec<char> = src.chars().collect();
    let (_toks, trivia) = tokenize_with_trivia(src);
    let mut out = Vec::new();
    for t in trivia {
        if t.kind != TriviaKind::LineComment {
            continue;
        }
        let start = t.start as usize;
        let (own_line, blank_before) = preceding_layout(&chars, start);
        out.push(Comment {
            text: t.text.trim_end().to_string(),
            start: t.start,
            line: t.line,
            own_line,
            blank_before,
        });
    }
    out
}

/// Scans backwards from a comment's start offset to determine whether it stands
/// alone on its line (`own_line`) and whether a blank line precedes it
/// (`blank_before`).
///
/// `own_line` is true when only spaces/tabs separate the comment from the start
/// of input or the previous newline. `blank_before` is true when, skipping the
/// comment's own (whitespace-only) leading run and the newline above it, the
/// line above is itself empty or whitespace-only.
fn preceding_layout(chars: &[char], start: usize) -> (bool, bool) {
    // Walk back over same-line leading whitespace.
    let mut i = start;
    while i > 0 {
        match chars[i - 1] {
            ' ' | '\t' | '\r' => i -= 1,
            _ => break,
        }
    }
    // If we hit the start of input or a newline, the comment is on its own line.
    let own_line = i == 0 || chars[i - 1] == '\n';
    if !own_line {
        return (false, false);
    }
    // `i` now sits just after the newline that ends the previous line (or at 0).
    // Determine whether that previous line is blank (whitespace only).
    if i == 0 {
        return (true, false);
    }
    // Step over the newline that begins our line.
    let mut j = i - 1; // index of the '\n'
                       // Handle a CRLF: the '\r' belongs to the previous line's end.
    if j > 0 && chars[j] == '\n' && chars[j - 1] == '\r' {
        j -= 1;
    }
    // Scan the previous line's content (from its start to `j`).
    let mut k = j;
    let mut blank = true;
    while k > 0 {
        match chars[k - 1] {
            '\n' => break,
            ' ' | '\t' | '\r' => k -= 1,
            _ => {
                blank = false;
                break;
            }
        }
    }
    (true, blank)
}
