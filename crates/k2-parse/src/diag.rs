//! Diagnostics produced by the parser.
//!
//! A parse never panics on malformed input (mirroring the lexer's never-panic
//! guarantee). Instead it records a [`Diagnostic`] and synchronizes to a safe
//! recovery point, so a single bad statement does not derail the rest of the
//! file. The full set of diagnostics is returned alongside the (possibly
//! partial) [`SourceFile`](k2_syntax::SourceFile) in a [`ParseResult`].

use k2_syntax::{Label, RichDiagnostic, RichSeverity, SourceFile, Span};

/// The severity of a [`Diagnostic`]. Only [`Severity::Error`] entries make a
/// parse "fail"; warnings are advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A hard parse error: the input did not match the grammar.
    Error,
    /// An advisory warning (e.g. a stray, ignored doc comment).
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

/// A single parser diagnostic: where it occurred and what went wrong.
///
/// The `span`/`severity`/`message` triple is the original, terse shape every
/// caller relies on. The `labels`/`notes`/`help` fields are *additive*: they
/// default to empty, so every existing constructor and every test that reads
/// `.message`/`.span` keeps working unchanged. A phase that wants a richer
/// report attaches secondary labels / notes / help via the chainable
/// `with_*` builders, and the driver renders the whole thing through
/// [`Diagnostic::to_rich`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The source span the diagnostic points at (its `line`/`col` give a
    /// human-readable location). This is the *primary* span.
    pub span: Span,
    /// The severity (error vs. warning).
    pub severity: Severity,
    /// A human-readable message (the rendered header).
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
