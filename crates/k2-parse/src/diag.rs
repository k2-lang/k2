//! Diagnostics produced by the parser.
//!
//! A parse never panics on malformed input (mirroring the lexer's never-panic
//! guarantee). Instead it records a [`Diagnostic`] and synchronizes to a safe
//! recovery point, so a single bad statement does not derail the rest of the
//! file. The full set of diagnostics is returned alongside the (possibly
//! partial) [`SourceFile`](k2_syntax::SourceFile) in a [`ParseResult`].

use k2_syntax::{SourceFile, Span};

/// The severity of a [`Diagnostic`]. Only [`Severity::Error`] entries make a
/// parse "fail"; warnings are advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A hard parse error: the input did not match the grammar.
    Error,
    /// An advisory warning (e.g. a stray, ignored doc comment).
    Warning,
}

/// A single parser diagnostic: where it occurred and what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The source span the diagnostic points at (its `line`/`col` give a
    /// human-readable location).
    pub span: Span,
    /// The severity (error vs. warning).
    pub severity: Severity,
    /// A human-readable message.
    pub message: String,
}

impl Diagnostic {
    /// Builds an error-severity diagnostic.
    pub fn error(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            message: message.into(),
        }
    }

    /// Builds a warning-severity diagnostic.
    pub fn warning(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

/// The result of a parse: the (best-effort, possibly partial) syntax tree plus
/// every diagnostic that was recorded. Callers decide success via
/// [`ParseResult::is_ok`].
#[derive(Clone, Debug, PartialEq)]
pub struct ParseResult {
    /// The parsed source file (always present, even on error).
    pub file: SourceFile,
    /// Every diagnostic recorded during the parse, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

impl ParseResult {
    /// `true` if the parse produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error)
    }

    /// An iterator over just the error-severity diagnostics.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
    }
}
