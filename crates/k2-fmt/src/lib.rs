//! # k2-fmt — the canonical source formatter for the k2 programming language
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate is the single source of truth for k2's canonical code style. It
//! parses `.k2` source through [`k2_parse`], recovers every comment through the
//! additive [`k2_lexer::tokenize_with_trivia`] side channel, and re-emits the
//! file in **one obvious form** — 4-space indent, same-line `{`, spaces around
//! binary operators, deterministic width-aware line breaking at 100 columns, and
//! lossless comment preservation.
//!
//! ## Design principle: one obvious way
//!
//! The formatter never makes a stylistic choice at runtime. Output is rebuilt
//! from the AST (plus recovered line comments); original inter-token whitespace
//! is discarded and regenerated. Literals are emitted **verbatim**. Parentheses
//! are re-inserted only where precedence/associativity require them, never more.
//!
//! ## Refuses to format broken input
//!
//! [`format_source`] parses first; if the source has any *error*-severity
//! diagnostic it returns those diagnostics unchanged rather than emit a
//! best-effort (and possibly wrong) reformatting. Warnings (e.g. a stray doc
//! comment) do not block formatting.
//!
//! ```
//! let src = "const  x=1 ;\n";
//! let out = k2_fmt::format_source(src).unwrap();
//! assert_eq!(out, "const x = 1;\n");
//! ```

mod comments;
mod printer;

pub use k2_parse::Diagnostic;

use k2_parse::Severity;
use printer::Printer;

/// Formats k2 `src` into its canonical form.
///
/// Parses `src` first; if there are any error-severity diagnostics, refuses to
/// format and returns just those errors. On success returns the canonical text,
/// which always ends in exactly one `\n`.
///
/// The transformation is whitespace/comments/parens-only: re-parsing the output
/// yields a structurally identical AST (the crate's round-trip test certifies
/// this), and every comment in the input survives.
pub fn format_source(src: &str) -> Result<String, Vec<Diagnostic>> {
    let result = k2_parse::parse(src);
    if !result.is_ok() {
        let errors: Vec<Diagnostic> = result
            .diagnostics
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        return Err(errors);
    }
    let comments = comments::collect_comments(src);
    let chars: Vec<char> = src.chars().collect();
    let printer = Printer::new(&result.file, &comments, &chars);
    Ok(printer.print_file(&result.file))
}

#[cfg(test)]
mod tests;
