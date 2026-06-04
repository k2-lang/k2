//! # Rich diagnostics — the shared, phase-agnostic diagnostic model
//!
//! Every phase of the compiler (lexer, parser, resolver, type checker, MIR
//! lowering, codegen) emits *diagnostics*: a message anchored at a source
//! [`Span`], with a severity. For most of the toolchain's life that was all a
//! diagnostic carried — a single span and a string — and the driver rendered it
//! as one terse `file:line:col: error: message` line.
//!
//! This module introduces a richer, rustc/ariadne-style model that a renderer
//! can turn into a labelled, multi-line report: a **primary** labelled span,
//! zero or more **secondary** labelled spans (each with its own message), zero
//! or more **notes**, and an optional **help/suggestion** line.
//!
//! The model lives in `k2-syntax` — the universal leaf every phase already
//! depends on for [`Span`] — so a *shared* rich shape needs zero new crate
//! edges. Each phase keeps its own light `Diagnostic` struct (`{ span,
//! severity, message }`, extended *additively* with optional `labels`/`notes`/
//! `help`); those convert *into* this [`RichDiagnostic`] for rendering. The
//! renderer itself (which needs the source text) lives in the driver, `k2c`.
//!
//! Nothing here allocates beyond the strings a diagnostic actually carries, and
//! the builder methods are `#[must_use]` so the common single-span case stays a
//! one-liner with the exact ergonomics of the old `Diagnostic::error(span, msg)`.

use crate::Span;

/// The severity of a [`RichDiagnostic`].
///
/// `Error` and `Warning` match the per-phase severities one-for-one; `Note` is
/// new and is used only for free-standing informational diagnostics (the
/// renderer also uses the *word* `note`/`help` for the trailing lines of a
/// single diagnostic, but those are not separate `Severity::Note` diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A hard error: the phase failed. Gates later phases / the build.
    Error,
    /// An advisory warning: surfaced but never fails the build.
    Warning,
    /// A free-standing informational note.
    Note,
}

impl Severity {
    /// The lowercase word the renderer prints in a diagnostic header
    /// (`error`/`warning`/`note`).
    pub fn word(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// Whether a [`Label`] is the diagnostic's single *primary* underline (drawn
/// with `^`) or one of its *secondary* underlines (drawn with `-`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelStyle {
    /// The primary span — the focus of the diagnostic. Rendered with `^`.
    Primary,
    /// A supporting span (e.g. "expected because of this"). Rendered with `-`.
    Secondary,
}

impl LabelStyle {
    /// The underline glyph for this style (`^` primary, `-` secondary).
    pub fn glyph(self) -> char {
        match self {
            LabelStyle::Primary => '^',
            LabelStyle::Secondary => '-',
        }
    }
}

/// One labelled span: a source range, an underline style, and an inline message
/// drawn at the end of the underline (which may be empty, drawing a bare
/// underline with no trailing text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// The source range this label underlines.
    pub span: Span,
    /// Primary (`^`) or secondary (`-`).
    pub style: LabelStyle,
    /// The inline message printed at the end of the underline (may be empty).
    pub message: String,
}

impl Label {
    /// Builds a primary label.
    pub fn primary(span: Span, message: impl Into<String>) -> Label {
        Label {
            span,
            style: LabelStyle::Primary,
            message: message.into(),
        }
    }

    /// Builds a secondary label.
    pub fn secondary(span: Span, message: impl Into<String>) -> Label {
        Label {
            span,
            style: LabelStyle::Secondary,
            message: message.into(),
        }
    }
}

/// A fully-featured diagnostic ready for rendering: a header severity/message, a
/// primary labelled span, any number of secondary labels, notes, and an
/// optional help line.
///
/// This is the *rendering* shape. Phases build their own light diagnostics and
/// convert into this via `to_rich`; the renderer (in `k2c`) consumes only this
/// plus the source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichDiagnostic {
    /// The diagnostic's severity (drives the header word and color).
    pub severity: Severity,
    /// The header message (identical to the legacy `Diagnostic::message`).
    pub message: String,
    /// The primary labelled span (its inline `message` may be empty).
    pub primary: Label,
    /// Zero or more secondary labels, each with its own span and message.
    pub secondary: Vec<Label>,
    /// Zero or more `note: …` lines printed below the snippet.
    pub notes: Vec<String>,
    /// An optional `help: …` suggestion printed last.
    pub help: Option<String>,
}

impl RichDiagnostic {
    /// Builds an error diagnostic with an (initially unlabelled) primary span.
    #[must_use]
    pub fn error(span: Span, message: impl Into<String>) -> RichDiagnostic {
        RichDiagnostic::new(Severity::Error, span, message)
    }

    /// Builds a warning diagnostic with an (initially unlabelled) primary span.
    #[must_use]
    pub fn warning(span: Span, message: impl Into<String>) -> RichDiagnostic {
        RichDiagnostic::new(Severity::Warning, span, message)
    }

    /// Builds a diagnostic of an explicit severity.
    #[must_use]
    pub fn new(severity: Severity, span: Span, message: impl Into<String>) -> RichDiagnostic {
        RichDiagnostic {
            severity,
            message: message.into(),
            primary: Label {
                span,
                style: LabelStyle::Primary,
                message: String::new(),
            },
            secondary: Vec::new(),
            notes: Vec::new(),
            help: None,
        }
    }

    /// Sets the inline message drawn under the primary span's underline.
    #[must_use]
    pub fn primary_label(mut self, message: impl Into<String>) -> RichDiagnostic {
        self.primary.message = message.into();
        self
    }

    /// Adds a secondary label (its own span + message).
    #[must_use]
    pub fn secondary(mut self, span: Span, message: impl Into<String>) -> RichDiagnostic {
        self.secondary.push(Label::secondary(span, message));
        self
    }

    /// Adds a `note: …` line.
    #[must_use]
    pub fn note(mut self, message: impl Into<String>) -> RichDiagnostic {
        self.notes.push(message.into());
        self
    }

    /// Sets the `help: …` suggestion line.
    #[must_use]
    pub fn help(mut self, message: impl Into<String>) -> RichDiagnostic {
        self.help = Some(message.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_assembles_full_diagnostic() {
        let prim = Span::new(10, 14, 1, 11);
        let sec = Span::new(0, 5, 1, 1);
        let d = RichDiagnostic::error(prim, "type mismatch")
            .primary_label("this is `bool`")
            .secondary(sec, "expected `i32` here")
            .note("the binding was declared `i32`")
            .help("convert with `@as(i32, …)`");

        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "type mismatch");
        assert_eq!(d.primary.span, prim);
        assert_eq!(d.primary.style, LabelStyle::Primary);
        assert_eq!(d.primary.message, "this is `bool`");
        assert_eq!(d.secondary.len(), 1);
        assert_eq!(d.secondary[0].style, LabelStyle::Secondary);
        assert_eq!(d.secondary[0].span, sec);
        assert_eq!(d.notes, vec!["the binding was declared `i32`".to_string()]);
        assert_eq!(d.help.as_deref(), Some("convert with `@as(i32, …)`"));
    }

    #[test]
    fn severity_words_and_glyphs() {
        assert_eq!(Severity::Error.word(), "error");
        assert_eq!(Severity::Warning.word(), "warning");
        assert_eq!(Severity::Note.word(), "note");
        assert_eq!(LabelStyle::Primary.glyph(), '^');
        assert_eq!(LabelStyle::Secondary.glyph(), '-');
    }
}
