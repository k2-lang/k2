//! Diagnostics produced by name resolution.
//!
//! Name resolution never panics on any *parseable* input: a malformed reference
//! becomes a [`Diagnostic`] and resolution continues, so a single undeclared
//! identifier does not derail the rest of the file. The diagnostic shape
//! deliberately mirrors `k2_parse::Diagnostic` (`{ span, severity, message }`)
//! so the driver can print resolution and parse diagnostics with one identical
//! `label:line:col: severity: message` formatter. We define our *own* copy here
//! rather than depend on `k2-parse`, keeping the resolver a leaf crate over
//! `k2-syntax` alone.

use k2_syntax::Span;

/// The severity of a [`Diagnostic`]. Only [`Severity::Error`] entries make a
/// resolution "fail"; warnings are advisory and never block the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A hard resolution error: an undeclared name, a duplicate or shadowing
    /// declaration, or a missing imported file.
    Error,
    /// An advisory warning that does not fail the build. Emitted for a
    /// structural import cycle (legal per spec §08 2.3, but surfaced for
    /// tooling).
    Warning,
}

/// A single resolution diagnostic: where it occurred and what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The source span the diagnostic points at. Its `line`/`col` give the
    /// human-readable location reused by the CLI.
    pub span: Span,
    /// The severity (error vs. warning).
    pub severity: Severity,
    /// A human-readable message, e.g. `use of undeclared identifier `zzz``.
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

    /// `true` if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}
