//! # k2-parse — the recursive-descent parser for the k2 programming language
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate turns the [`Token`](k2_lexer::Token) stream produced by
//! [`k2_lexer`] into the [`SourceFile`](k2_syntax::SourceFile) AST defined by
//! [`k2_syntax`]. It is a hand-written recursive-descent parser that follows
//! `docs/grammar.ebnf` directly: one function per grammar production, with the
//! expression precedence cascade laid out lowest-to-highest.
//!
//! ## Types are expressions
//!
//! k2 writes types in postfix-modifier form (`?T`, `*const T`, `[]T`, `[N]T`,
//! `E!T`, `fn (params) Ret`, `error { ... }`, `struct { ... }`). The parser
//! represents every type as an ordinary [`Expr`](k2_syntax::Expr): the
//! type-constructor prefixes live in [`Parser::parse_unary`], and the only
//! type-specific wrapper is [`Parser::parse_type`], which applies the infix
//! `E!T` error-union form in type position.
//!
//! ## Never panics
//!
//! Consistent with the lexer, the parser never panics on malformed input. A
//! syntax error becomes a [`Diagnostic`] and the parser synchronizes to the
//! next `;`/`}` (panic-mode recovery), so multiple errors are reported in one
//! pass. [`parse`] always returns a [`ParseResult`] holding a best-effort tree
//! plus the diagnostics.
//!
//! ```
//! let res = k2_parse::parse("const std = @import(\"std\");\n");
//! assert!(res.is_ok());
//! assert_eq!(res.file.items.len(), 1);
//! ```

mod diag;
mod expr;
mod item;
mod pretty;
mod stmt;
mod types;

pub use diag::{Diagnostic, ParseResult, Severity};
pub use pretty::{to_sexpr, to_sexpr_spans};

use k2_lexer::{tokenize, Token, TokenKind};
use k2_syntax::Span;

/// Parses a complete `.k2` source string into a [`ParseResult`].
///
/// The returned [`ParseResult::file`] is always populated (best-effort, even on
/// error); [`ParseResult::is_ok`] reports whether any error-severity diagnostic
/// was recorded.
pub fn parse(src: &str) -> ParseResult {
    let toks = tokenize(src);
    let line_starts = build_line_starts(src);
    let src_len = src.chars().count() as u32;
    let mut p = Parser {
        src_len,
        line_starts,
        toks,
        pos: 0,
        diags: Vec::new(),
        no_struct_lit: false,
        depth: 0,
        depth_exceeded: false,
    };
    let file = p.parse_source_file();
    ParseResult {
        file,
        diagnostics: p.diags,
    }
}

/// Builds the `line_starts` table: `line_starts[n]` is the scalar offset of the
/// first character of 1-based line `n + 1`. Line 1 starts at offset 0; after
/// every `'\n'` the next character begins a new line. Built over `src.chars()`
/// so the offsets are *scalar* indices, matching the lexer's column counting.
fn build_line_starts(src: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut offset: u32 = 0;
    for c in src.chars() {
        offset += 1;
        if c == '\n' {
            starts.push(offset);
        }
    }
    starts
}

/// The recursive-descent parser over a lexed `.k2` token stream.
///
/// The cursor (`pos`) indexes into `toks`, which always ends with an `Eof`
/// token. Byte/scalar offsets for spans are reconstructed from each token's
/// 1-based `line`/`col` via the `line_starts` table — *not* by summing token
/// text, which would be blind to inter-token whitespace and comments.
pub struct Parser {
    /// Total length of the source in scalars (the end offset of `Eof`).
    src_len: u32,
    /// Scalar offset of the first character of each 1-based line.
    line_starts: Vec<u32>,
    /// The token stream, including retained `DocComment`s and a trailing `Eof`.
    toks: Vec<Token>,
    /// Cursor: index of the next unconsumed token.
    pos: usize,
    /// Recorded diagnostics, in source order.
    diags: Vec<Diagnostic>,
    /// Context flag: when set, a `{` is *not* read as a typed initializer
    /// (`T{...}`) — it belongs to a following block. Set while parsing the
    /// header expression of `if`/`while`/`for`/`switch`; cleared inside parens.
    no_struct_lit: bool,
    /// Current recursion depth across the mutually-recursive descent functions
    /// (expression cascade, unary, primary/grouped, postfix, type entry,
    /// statement, block, container/member). Guarded by [`Parser::enter`] so a
    /// grammar-valid but pathologically deep input cannot overflow the native
    /// stack (a stack overflow is an *uncatchable* `SIGABRT`, which would break
    /// the crate's "never panics" contract). See [`MAX_DEPTH`].
    depth: u32,
    /// Latches once [`MAX_DEPTH`] has been hit so the "nesting too deep"
    /// diagnostic is emitted exactly *once* per parse, not at every over-deep
    /// frame on the way back up.
    depth_exceeded: bool,
}

/// The maximum recursion depth permitted across the mutually-recursive descent
/// functions before the parser bails with a single "nesting too deep"
/// diagnostic instead of recursing further.
///
/// Chosen well below what the default 8 MiB stack tolerates with a wide margin.
/// One unit of `depth` spans the deepest single grammar step — a parenthesized
/// sub-expression runs the *entire* precedence cascade (≈14 frames) for each
/// `(`, so the cap is set so even that worst case stays comfortably inside the
/// smallest stack the parser realistically runs on (Rust's 2 MiB test-harness
/// worker threads, well under the 8 MiB main thread). `128` admits any
/// realistic hand-written source while reserving a large safety margin. The cap
/// is enforced *structurally* (see [`Parser::enter`]) because a stack overflow
/// cannot be caught and recovered from; bounding the tree here also bounds the
/// depth the pretty-printer must recurse over.
const MAX_DEPTH: u32 = 128;

/// An RAII recursion-depth guard. Returned by [`Parser::enter`] at the top of
/// each recursive descent function and held in a local for the duration of that
/// function; its `Drop` decrements the parser's depth counter so *every* return
/// path (early returns, `?`, or falling off the end) restores the depth.
///
/// The guard deliberately does **not** borrow the `Parser`: it carries a raw
/// pointer to the single `depth` counter so the descent function can keep using
/// `&mut self` freely while the guard is alive. The pointer is always valid —
/// the guard never outlives the `&mut self` borrow that produced it, and only
/// ever reads/writes the `u32` `depth` field — so the `unsafe` is sound. (A
/// safe `&mut u32` field-guard would borrow `self` for the whole body and make
/// the descent functions unwritable, so the raw pointer is unavoidable here.)
struct DepthGuard {
    depth: *mut u32,
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        // SAFETY: `depth` points at the live `Parser::depth` field; the guard is
        // dropped before the `&mut Parser` that created it is released.
        unsafe {
            *self.depth -= 1;
        }
    }
}

impl Parser {
    // ---- span reconstruction --------------------------------------------

    /// The scalar offset of the first character of token `i` (clamped to the
    /// final `Eof`, which sits at end of input).
    fn start_offset(&self, i: usize) -> u32 {
        let tok = &self.toks[i];
        if tok.kind == TokenKind::Eof {
            return self.src_len;
        }
        let line_idx = (tok.line as usize).saturating_sub(1);
        let base = self
            .line_starts
            .get(line_idx)
            .copied()
            .unwrap_or(self.src_len);
        base + (tok.col.saturating_sub(1))
    }

    /// The span of token `i`. For `Eof` this is a zero-width span at end of
    /// input. The end offset adds the token's scalar length (which, for
    /// multi-line tokens like multiline strings, includes embedded newlines and
    /// therefore stays a correct global offset).
    fn span_of(&self, i: usize) -> Span {
        let tok = &self.toks[i];
        let start = self.start_offset(i);
        if tok.kind == TokenKind::Eof {
            return Span::point(start, tok.line, tok.col);
        }
        let len = tok.text.chars().count() as u32;
        Span::new(start, start + len, tok.line, tok.col)
    }

    // ---- recursion-depth guard -------------------------------------------

    /// Enters one level of recursive descent. Increments the depth counter and
    /// returns `(guard, over_limit)`:
    ///
    /// * `guard` must be held in a local for the rest of the function; dropping
    ///   it decrements the depth on every return path.
    /// * `over_limit` is `true` once [`MAX_DEPTH`] has been reached. A caller
    ///   that sees `true` must **not** recurse further — it emits/relies on the
    ///   single "nesting too deep" diagnostic (via [`Parser::too_deep`]) and
    ///   returns an error/empty node instead.
    ///
    /// This structural bound is what keeps a grammar-valid but pathologically
    /// deep input from overflowing the native stack (an uncatchable `SIGABRT`).
    #[must_use]
    fn enter(&mut self) -> (DepthGuard, bool) {
        self.depth += 1;
        let guard = DepthGuard {
            depth: &mut self.depth as *mut u32,
        };
        // Bail when this frame crosses the cap *or* once the cap has already been
        // crossed earlier in the parse. Latching on `depth_exceeded` collapses
        // the descent immediately on every recursive entry instead of letting it
        // climb back to the cap, bail, unwind one level, and re-descend on the
        // same still-deep input — which would build an unbounded tree (and then
        // overflow on the tree's recursive `Drop`/print) despite the per-frame
        // bound. After the latch, the whole parse winds down to recovery quickly.
        (guard, self.depth > MAX_DEPTH || self.depth_exceeded)
    }

    /// Records the single "nesting too deep" diagnostic (only the first time the
    /// cap is crossed) and returns an error-node expression at the current
    /// token, used by the recursive descent functions as their bail-out value.
    fn too_deep(&mut self) -> k2_syntax::Expr {
        let span = self.here();
        if !self.depth_exceeded {
            self.depth_exceeded = true;
            self.error(span, "nesting too deep");
        }
        k2_syntax::Expr::Ident {
            name: "<error>".to_string(),
            span,
        }
    }

    // ---- cursor helpers --------------------------------------------------

    /// The current token.
    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }

    /// The kind of the current token.
    fn cur_kind(&self) -> TokenKind {
        self.toks[self.pos].kind
    }

    /// The kind of the token `n` positions ahead of the cursor (clamped to the
    /// trailing `Eof`).
    fn peek_kind(&self, n: usize) -> TokenKind {
        let i = (self.pos + n).min(self.toks.len() - 1);
        self.toks[i].kind
    }

    /// `true` if the current token is of kind `k`.
    fn at(&self, k: TokenKind) -> bool {
        self.cur_kind() == k
    }

    /// `true` if the parser has consumed every token (cursor on `Eof`).
    fn at_eof(&self) -> bool {
        self.cur_kind() == TokenKind::Eof
    }

    /// The span of the current token, for anchoring node spans / diagnostics.
    fn here(&self) -> Span {
        self.span_of(self.pos)
    }

    /// Consumes the current token and returns its span. Stops advancing past
    /// the trailing `Eof`.
    fn bump(&mut self) -> Span {
        let span = self.here();
        if !self.at_eof() {
            self.pos += 1;
        }
        span
    }

    /// Consumes the current token if it is of kind `k`, returning its span.
    fn eat(&mut self, k: TokenKind) -> Option<Span> {
        if self.at(k) {
            Some(self.bump())
        } else {
            None
        }
    }

    /// Consumes a token of kind `k`, or records an error and returns a point
    /// span at the current token for recovery (without consuming it).
    fn expect(&mut self, k: TokenKind, ctx: &str) -> Span {
        if self.at(k) {
            self.bump()
        } else {
            let span = self.here();
            self.error(
                span,
                format!("expected {} {ctx}, found {}", describe(k), self.cur_desc()),
            );
            Span::point(span.start, span.line, span.col)
        }
    }

    /// A short human-readable description of the current token for messages.
    fn cur_desc(&self) -> String {
        let tok = self.cur();
        if tok.kind == TokenKind::Eof {
            "end of input".to_string()
        } else {
            format!("{} `{}`", describe(tok.kind), render_text(&tok.text))
        }
    }

    // ---- doc comments ----------------------------------------------------

    /// Consumes any consecutive leading `DocComment` tokens and joins them with
    /// newlines, returning `None` if there were none.
    fn take_doc(&mut self) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        while self.at(TokenKind::DocComment) {
            lines.push(self.cur().text.clone());
            self.bump();
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// Skips a stray `DocComment` that appears where no declaration follows,
    /// emitting a warning. Used by statement/expression code that should not see
    /// doc comments. Returns `true` if one was skipped.
    fn skip_stray_doc(&mut self) -> bool {
        if self.at(TokenKind::DocComment) {
            let span = self.here();
            self.warning(span, "doc comment attached to nothing; ignoring");
            self.bump();
            true
        } else {
            false
        }
    }

    // ---- diagnostics & recovery -----------------------------------------

    /// Records an error-severity diagnostic.
    fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.push(Diagnostic::error(span, msg));
    }

    /// Records a warning-severity diagnostic.
    fn warning(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.push(Diagnostic::warning(span, msg));
    }

    /// A placeholder expression used when an expression could not be parsed.
    /// Records an error and produces a shaped-but-empty node so the tree stays
    /// well-formed and recovery can continue.
    fn error_expr(&mut self, span: Span, msg: impl Into<String>) -> k2_syntax::Expr {
        self.error(span, msg);
        k2_syntax::Expr::Ident {
            name: "<error>".to_string(),
            span,
        }
    }

    /// Panic-mode recovery inside a statement: skip tokens until just past the
    /// next `;`, or up to (not consuming) a closing `}`/`Eof`, or to a token
    /// that clearly starts a new statement. Always makes progress.
    fn synchronize_stmt(&mut self) {
        // Reaching a recovery boundary means we have unwound out of the deep
        // region, so clear the depth latch: subsequent statements/items parse
        // normally again (the latch only collapses the *current* over-deep
        // descent, it must not poison the rest of the file).
        self.depth_exceeded = false;
        // Guarantee progress even if the cursor sits on a sync point already.
        if !self.at_eof() && !self.at(TokenKind::RBrace) {
            self.bump();
        }
        while !self.at_eof() {
            match self.cur_kind() {
                TokenKind::Semicolon => {
                    self.bump();
                    return;
                }
                TokenKind::RBrace => return,
                TokenKind::KwConst
                | TokenKind::KwVar
                | TokenKind::KwReturn
                | TokenKind::KwIf
                | TokenKind::KwWhile
                | TokenKind::KwFor
                | TokenKind::KwSwitch
                | TokenKind::KwBreak
                | TokenKind::KwContinue
                | TokenKind::KwDefer
                | TokenKind::KwErrdefer
                | TokenKind::KwComptime => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Panic-mode recovery at item scope: skip to the next top-level starter or
    /// `Eof`. Always makes progress.
    fn synchronize_item(&mut self) {
        // See `synchronize_stmt`: clear the depth latch at the recovery boundary
        // so later top-level items are not poisoned by one over-deep expression.
        self.depth_exceeded = false;
        if !self.at_eof() {
            self.bump();
        }
        while !self.at_eof() {
            match self.cur_kind() {
                TokenKind::KwPub
                | TokenKind::KwConst
                | TokenKind::KwVar
                | TokenKind::KwFn
                | TokenKind::KwExtern
                | TokenKind::KwExport
                | TokenKind::KwInline
                | TokenKind::KwTest
                | TokenKind::KwComptime
                | TokenKind::DocComment => return,
                _ => {
                    self.bump();
                }
            }
        }
    }
}

/// Maps an [`AssignOp`](k2_syntax::AssignOp) from an assignment token kind.
/// Returns `None` for any non-assignment token.
fn assign_op_of(k: TokenKind) -> Option<k2_syntax::AssignOp> {
    use k2_syntax::AssignOp::*;
    use TokenKind as T;
    Some(match k {
        T::Eq => Eq,
        T::PlusEq => AddEq,
        T::MinusEq => SubEq,
        T::StarEq => MulEq,
        T::SlashEq => DivEq,
        T::PercentEq => RemEq,
        T::AmpEq => AndEq,
        T::PipeEq => OrEq,
        T::CaretEq => XorEq,
        T::ShlEq => ShlEq,
        T::ShrEq => ShrEq,
        _ => return None,
    })
}

/// A short, stable, human-readable name for a token kind, for diagnostics.
fn describe(k: TokenKind) -> &'static str {
    use TokenKind::*;
    match k {
        Ident => "identifier",
        Builtin => "builtin",
        EscapedIdent => "escaped identifier",
        IntLiteral => "integer literal",
        FloatLiteral => "float literal",
        CharLiteral => "character literal",
        StringLiteral => "string literal",
        MultilineString => "multiline string",
        LParen => "`(`",
        RParen => "`)`",
        LBrace => "`{`",
        RBrace => "`}`",
        LBracket => "`[`",
        RBracket => "`]`",
        Comma => "`,`",
        Semicolon => "`;`",
        Colon => "`:`",
        Eq => "`=`",
        FatArrow => "`=>`",
        Bang => "`!`",
        Pipe => "`|`",
        Dot => "`.`",
        DotDot => "`..`",
        DotDotDot => "`...`",
        Eof => "end of input",
        DocComment => "doc comment",
        Error => "invalid token",
        _ => "token",
    }
}

/// Escapes a token's text for inclusion in a one-line diagnostic message.
fn render_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `src`, asserting it produced no error diagnostics, and returns the
    /// file.
    fn ok(src: &str) -> k2_syntax::SourceFile {
        let res = parse(src);
        assert!(
            res.is_ok(),
            "expected clean parse of {src:?}, got: {:?}",
            res.diagnostics
        );
        res.file
    }

    #[test]
    fn line_starts_are_scalar_offsets() {
        let starts = build_line_starts("a\nbb\nccc");
        assert_eq!(starts, vec![0, 2, 5]);
    }

    #[test]
    fn offset_reconstruction_after_comment_and_blank_line() {
        // The `const` keyword starts after a `//` comment line and a blank line.
        let src = "// hi\n\nconst x = 1;\n";
        let res = parse(src);
        assert!(res.is_ok(), "{:?}", res.diagnostics);
        let item = &res.file.items[0];
        // "// hi\n\n" is 7 scalars, so `const` begins at offset 7.
        assert_eq!(item.span().start, 7);
        assert_eq!(item.span().line, 3);
    }

    #[test]
    fn empty_source_parses_clean() {
        let f = ok("");
        assert!(f.items.is_empty());
        ok("   \n\t  \n");
    }

    #[test]
    fn const_decl_minimal() {
        let f = ok("const x = 1;");
        assert_eq!(f.items.len(), 1);
    }

    #[test]
    fn missing_semicolon_reports_one_error_and_recovers() {
        // The first decl misses its `;`; the second should still parse.
        let res = parse("const a = 1\nconst b = 2;\n");
        assert!(!res.is_ok());
        assert_eq!(res.errors().count(), 1, "{:?}", res.diagnostics);
        assert_eq!(res.file.items.len(), 2);
    }

    #[test]
    fn malformed_input_never_hangs() {
        // A pile of stray tokens must terminate with diagnostics, not a hang.
        let res = parse("} ] ) | & ^ => ... :: const");
        assert!(!res.is_ok());
    }

    #[test]
    fn deeply_nested_parens_do_not_overflow() {
        // Regression for the stack-overflow blocker: paren depth far beyond
        // MAX_DEPTH must return diagnostics (a normal parse error), never abort
        // the process via an uncatchable stack overflow. The depth guard bounds
        // BOTH the descent and the produced tree, so parsing AND pretty-printing
        // stay within even the (small ~2 MiB) test-harness worker-thread stack —
        // a missing guard would `SIGABRT` this binary instead of asserting.
        let depth = 100_000;
        let src = format!("const x = {}1{};\n", "(".repeat(depth), ")".repeat(depth));
        let res = parse(&src);
        assert!(!res.is_ok(), "deep nesting should be a parse error");
        assert_eq!(
            res.errors()
                .filter(|d| d.message.contains("nesting too deep"))
                .count(),
            1,
            "exactly one 'nesting too deep' diagnostic expected"
        );
        // The produced (bounded) tree must also pretty-print without overflow.
        let _ = to_sexpr(&res.file);
    }

    #[test]
    fn deeply_nested_blocks_do_not_overflow() {
        // The block / statement descent path is guarded the same way.
        let depth = 100_000;
        let src = format!("fn f() void {}{}\n", "{".repeat(depth), "}".repeat(depth));
        let res = parse(&src);
        assert!(!res.is_ok(), "deep block nesting should be a parse error");
        let _ = to_sexpr(&res.file);
    }

    #[test]
    fn deeply_nested_unary_does_not_overflow() {
        // Deep prefix unary `-----…1` exercises the unary descent guard.
        let depth = 100_000;
        let src = format!("const x = {}1;\n", "-".repeat(depth));
        let res = parse(&src);
        assert!(!res.is_ok(), "deep unary nesting should be a parse error");
        let _ = to_sexpr(&res.file);
    }
}
