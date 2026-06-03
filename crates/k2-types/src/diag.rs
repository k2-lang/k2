//! Diagnostics produced by type checking.
//!
//! The type checker never panics on any *parseable, resolvable* input: a real
//! type error becomes a [`Diagnostic`] and checking continues, so a single
//! mismatch does not derail the rest of the file. The diagnostic shape
//! deliberately mirrors `k2_resolve::Diagnostic` and `k2_parse::Diagnostic`
//! (`{ span, severity, message }`) so the driver can print parse, resolution,
//! and type diagnostics with one identical `label:line:col: severity: message`
//! formatter. We define our *own* copy here rather than depend on `k2-resolve`'s
//! diag module, keeping this crate a leaf over `k2-syntax` + `k2-resolve`,
//! exactly as `k2-resolve` did over `k2-parse`.

use k2_syntax::Span;

/// The severity of a [`Diagnostic`]. Only [`Severity::Error`] entries make a
/// type-check "fail"; warnings are advisory and never block the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A hard type error: a coercion failure, an arity mismatch, a non-bool
    /// condition, a non-exhaustive switch, and the rest of the concrete-core
    /// error catalogue.
    Error,
    /// An advisory warning that does not fail the build (e.g. an unreachable
    /// `else` arm on an already-exhaustive switch).
    Warning,
}

/// A single type diagnostic: where it occurred and what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The source span the diagnostic points at. Its `line`/`col` give the
    /// human-readable location reused by the CLI.
    pub span: Span,
    /// The severity (error vs. warning).
    pub severity: Severity,
    /// A human-readable message, e.g. ``expected `i32`, found `bool` ``.
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
