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

use k2_syntax::{Label, RichDiagnostic, RichSeverity, Span};

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

impl From<Severity> for RichSeverity {
    fn from(s: Severity) -> RichSeverity {
        match s {
            Severity::Error => RichSeverity::Error,
            Severity::Warning => RichSeverity::Warning,
        }
    }
}

/// A single type diagnostic: where it occurred and what went wrong.
///
/// The `labels`/`notes`/`help` fields are *additive* (default empty), so every
/// existing construction and every `.message`/`.span` assertion keeps working;
/// the high-value diagnostics opt into a richer report via the chainable
/// `with_*` builders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The source span the diagnostic points at. Its `line`/`col` give the
    /// human-readable location reused by the CLI. This is the *primary* span.
    pub span: Span,
    /// The severity (error vs. warning).
    pub severity: Severity,
    /// A human-readable message, e.g. ``expected `i32`, found `bool` ``.
    pub message: String,
    /// Optional inline label drawn under the primary span (empty = none).
    pub primary_label: String,
    /// Zero or more secondary labels (own span + message).
    pub labels: Vec<Label>,
    /// Zero or more `note: …` lines.
    pub notes: Vec<String>,
    /// An optional `help: …` suggestion.
    pub help: Option<String>,
}

impl Diagnostic {
    /// Builds an error-severity diagnostic (no labels/notes/help).
    pub fn error(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            message: message.into(),
            primary_label: String::new(),
            labels: Vec::new(),
            notes: Vec::new(),
            help: None,
        }
    }

    /// Builds a warning-severity diagnostic (no labels/notes/help).
    pub fn warning(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Warning,
            message: message.into(),
            primary_label: String::new(),
            labels: Vec::new(),
            notes: Vec::new(),
            help: None,
        }
    }

    /// `true` if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Sets the inline label drawn under the primary span's underline.
    #[must_use]
    pub fn with_primary_label(mut self, message: impl Into<String>) -> Diagnostic {
        self.primary_label = message.into();
        self
    }

    /// Appends a secondary label (its own span + message).
    #[must_use]
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push(Label::secondary(span, message));
        self
    }

    /// Appends a `note: …` line.
    #[must_use]
    pub fn with_note(mut self, message: impl Into<String>) -> Diagnostic {
        self.notes.push(message.into());
        self
    }

    /// Sets the `help: …` suggestion line.
    #[must_use]
    pub fn with_help(mut self, message: impl Into<String>) -> Diagnostic {
        self.help = Some(message.into());
        self
    }

    /// Converts into the shared [`RichDiagnostic`] rendering shape.
    pub fn to_rich(&self) -> RichDiagnostic {
        RichDiagnostic {
            severity: self.severity.into(),
            message: self.message.clone(),
            primary: Label::primary(self.span, self.primary_label.clone()),
            secondary: self.labels.clone(),
            notes: self.notes.clone(),
            help: self.help.clone(),
        }
    }
}
