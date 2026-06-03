//! # k2-lexer — the lexer for the k2 programming language
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate turns raw `.k2` source text (UTF-8 bytes) into a stream of
//! [`Token`]s. It is a faithful, standalone implementation of
//! `docs/spec/01-lexical-structure.md` and the "Lexical terminals" section of
//! `docs/grammar.ebnf`, which are co-normative with this code.
//!
//! Consistent with k2's **no ambient authority** pillar, the lexer is a *pure*
//! function from bytes to tokens: it performs no I/O, no allocation beyond the
//! output vector, and no `comptime` evaluation. It never panics on malformed
//! input — a lexical error becomes a [`TokenKind::Error`] token carrying a
//! message, so the parser can recover and report precisely.
//!
//! ## What the lexer produces
//!
//! * Whitespace (`U+0009`, `U+000A`, `U+000D`, `U+0020`) and `//` line comments
//!   are consumed and produce **no** token.
//! * `///` doc comments are *retained* as [`TokenKind::DocComment`] tokens.
//! * Everything else becomes exactly one token kind from [`TokenKind`].
//! * A final [`TokenKind::Eof`] token always terminates the stream.
//!
//! ```
//! use k2_lexer::{tokenize, TokenKind};
//!
//! let toks = tokenize("const x = 1;");
//! let kinds: Vec<_> = toks.iter().map(|t| t.kind).collect();
//! assert_eq!(
//!     kinds,
//!     vec![
//!         TokenKind::KwConst,
//!         TokenKind::Ident,
//!         TokenKind::Eq,
//!         TokenKind::IntLiteral,
//!         TokenKind::Semicolon,
//!         TokenKind::Eof,
//!     ]
//! );
//! ```

use std::fmt;

/// The kind of a single lexical token.
///
/// The variants are grouped to mirror the sections of the lexical-structure
/// spec: identifiers/builtins, the reserved keywords, literals, operators,
/// punctuation, and the retained doc comment. Two synthetic kinds — [`Eof`] and
/// [`Error`] — frame the stream and carry recovery information respectively.
///
/// [`Eof`]: TokenKind::Eof
/// [`Error`]: TokenKind::Error
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    // ---- Identifiers & builtins (spec §4) --------------------------------
    /// An ordinary identifier, including bare `_` and predeclared type names
    /// such as `i32`, `usize`, `bool`, `type`, `anyerror` (those are
    /// *identifiers*, not keywords, resolved later during name resolution).
    Ident,
    /// A compile-time builtin: `@name`, e.g. `@import`, `@TypeOf`, `@sizeOf`.
    Builtin,
    /// An escaped identifier: `@"..."`, used to spell a keyword or a
    /// non-conforming C symbol as a name.
    EscapedIdent,

    // ---- Keywords (spec §5) — the locked set of 35 -----------------------
    KwConst,
    KwVar,
    KwPub,
    KwFn,
    KwComptime,
    KwReturn,
    KwStruct,
    KwEnum,
    KwUnion,
    KwError,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwSwitch,
    KwBreak,
    KwContinue,
    KwDefer,
    KwErrdefer,
    KwTry,
    KwCatch,
    KwOrelse,
    KwAnd,
    KwOr,
    KwNot,
    KwUnreachable,
    KwTest,
    KwExtern,
    KwExport,
    KwInline,
    KwAlign,
    KwTrue,
    KwFalse,
    KwNull,
    KwUndefined, // (35th and final keyword in the locked charter set)

    // ---- Literals (spec §6) ----------------------------------------------
    /// Integer literal (`123`, `0xFF_FF`, `0o755`, `0b1010`).
    IntLiteral,
    /// Float literal (`3.14`, `6.022e23`, `0x1.8p3`).
    FloatLiteral,
    /// Single-quoted character literal (`'a'`, `'\n'`).
    CharLiteral,
    /// Double-quoted string literal (`"hello\n"`).
    StringLiteral,
    /// A multiline string literal: one or more `\\`-prefixed lines (spec §6.5).
    MultilineString,

    // ---- Operators (spec §7.1) -------------------------------------------
    Plus,        // +
    Minus,       // -
    Star,        // *
    Slash,       // /
    Percent,     // %
    Amp,         // &
    Pipe,        // |
    Caret,       // ^
    Tilde,       // ~
    Shl,         // <<
    Shr,         // >>
    EqEq,        // ==
    NotEq,       // !=
    Lt,          // <
    LtEq,        // <=
    Gt,          // >
    GtEq,        // >=
    Eq,          // =
    PlusEq,      // +=
    MinusEq,     // -=
    StarEq,      // *=
    SlashEq,     // /=
    PercentEq,   // %=
    AmpEq,       // &=
    PipeEq,      // |=
    CaretEq,     // ^=
    ShlEq,       // <<=
    ShrEq,       // >>=
    PlusPlus,    // ++  (comptime concat)
    Bang,        // !   (error-union type constructor ONLY — never boolean not)
    Question,    // ?   (optional type constructor)
    DotStar,     // .*  (pointer deref, postfix)
    DotQuestion, // .?  (optional unwrap, postfix)
    Dot,         // .   (field/member access)
    DotDot,      // ..  (range / slice)
    DotDotDot,   // ... (inclusive switch range)
    FatArrow,    // =>  (switch arm)
    // Note: there is deliberately NO `Arrow` (`->`) token. Per spec §7.1, `->`
    // is not part of k2's symbolic token set; `-` is only prefix negation or
    // infix subtraction, and `=>` (FatArrow) is the sole arrow-shaped token.

    // ---- Punctuation (spec §7.1) -----------------------------------------
    LParen,    // (
    RParen,    // )
    LBrace,    // {
    RBrace,    // }
    LBracket,  // [
    RBracket,  // ]
    Comma,     // ,
    Semicolon, // ;
    Colon,     // :

    // ---- Retained trivia & framing ---------------------------------------
    /// A `///` doc comment, retained so documentation tooling and `comptime`
    /// reflection can read it. The token text includes the leading `///`.
    DocComment,
    /// End-of-input marker; always the final token.
    Eof,
    /// A lexical error (e.g. an unterminated string, a stray `\r`, a NUL byte).
    /// The token text is the offending lexeme; recovery continues afterward.
    Error,
}

impl TokenKind {
    /// Returns `true` if this kind is one of the reserved keywords from the
    /// locked charter set.
    pub fn is_keyword(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            KwConst
                | KwVar
                | KwPub
                | KwFn
                | KwComptime
                | KwReturn
                | KwStruct
                | KwEnum
                | KwUnion
                | KwError
                | KwIf
                | KwElse
                | KwWhile
                | KwFor
                | KwSwitch
                | KwBreak
                | KwContinue
                | KwDefer
                | KwErrdefer
                | KwTry
                | KwCatch
                | KwOrelse
                | KwAnd
                | KwOr
                | KwNot
                | KwUnreachable
                | KwTest
                | KwExtern
                | KwExport
                | KwInline
                | KwAlign
                | KwTrue
                | KwFalse
                | KwNull
                | KwUndefined
        )
    }

    /// Returns `true` if this kind is a literal (int, float, char, string,
    /// multiline string). The literal *keywords* `true`/`false`/`null`/
    /// `undefined` are keywords, not literal kinds (see spec §6.7).
    pub fn is_literal(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            IntLiteral | FloatLiteral | CharLiteral | StringLiteral | MultilineString
        )
    }
}

/// Maps a candidate identifier spelling to its keyword [`TokenKind`], or
/// returns `None` if the word is an ordinary identifier.
///
/// This is the single source of truth for the locked charter keyword set; the
/// `keyword_kind_round_trips` test asserts every entry round-trips.
pub fn keyword_kind(word: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match word {
        "const" => KwConst,
        "var" => KwVar,
        "pub" => KwPub,
        "fn" => KwFn,
        "comptime" => KwComptime,
        "return" => KwReturn,
        "struct" => KwStruct,
        "enum" => KwEnum,
        "union" => KwUnion,
        "error" => KwError,
        "if" => KwIf,
        "else" => KwElse,
        "while" => KwWhile,
        "for" => KwFor,
        "switch" => KwSwitch,
        "break" => KwBreak,
        "continue" => KwContinue,
        "defer" => KwDefer,
        "errdefer" => KwErrdefer,
        "try" => KwTry,
        "catch" => KwCatch,
        "orelse" => KwOrelse,
        "and" => KwAnd,
        "or" => KwOr,
        "not" => KwNot,
        "unreachable" => KwUnreachable,
        "test" => KwTest,
        "extern" => KwExtern,
        "export" => KwExport,
        "inline" => KwInline,
        "align" => KwAlign,
        "true" => KwTrue,
        "false" => KwFalse,
        "null" => KwNull,
        "undefined" => KwUndefined,
        _ => return None,
    })
}

/// A single lexical token: its [`kind`], the exact source `text` it spans, and
/// its 1-based `line`/`col` start position for diagnostics.
///
/// [`kind`]: Token::kind
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// What sort of token this is.
    pub kind: TokenKind,
    /// The verbatim source slice this token spans (owned for ergonomics; the
    /// front-end is not performance-critical at lexing).
    pub text: String,
    /// 1-based line of the token's first character.
    pub line: u32,
    /// 1-based column (in Unicode scalar values) of the token's first character.
    pub col: u32,
}

impl Token {
    /// Constructs a token. Mostly used internally and by tests.
    pub fn new(kind: TokenKind, text: impl Into<String>, line: u32, col: u32) -> Token {
        Token {
            kind,
            text: text.into(),
            line,
            col,
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:>4}:{:<3} {:?} {:?}",
            self.line, self.col, self.kind, self.text
        )
    }
}

/// The lexer state machine over a decoded `.k2` source string.
///
/// Construct one with [`Lexer::new`] and drive it either by calling
/// [`Lexer::next_token`] in a loop, by iterating it (it implements
/// [`Iterator`], yielding tokens *up to and including* the final [`Eof`]), or
/// most simply via the free [`tokenize`] function.
///
/// [`Eof`]: TokenKind::Eof
pub struct Lexer {
    /// Source decoded to a vector of scalars for O(1) indexed lookahead.
    chars: Vec<char>,
    /// Current scan position as an index into `chars`.
    pos: usize,
    /// 1-based current line.
    line: u32,
    /// 1-based current column.
    col: u32,
    /// Set once the final `Eof` token has been emitted, so the `Iterator`
    /// terminates rather than yielding `Eof` forever.
    done: bool,
}

impl Lexer {
    /// Creates a lexer over `src`. A leading UTF-8 BOM (`U+FEFF`) is discarded,
    /// matching spec §1.2.
    pub fn new(src: &str) -> Lexer {
        let mut chars: Vec<char> = src.chars().collect();
        if chars.first() == Some(&'\u{FEFF}') {
            chars.remove(0);
        }
        Lexer {
            chars,
            pos: 0,
            line: 1,
            col: 1,
            done: false,
        }
    }

    // ---- low-level cursor helpers ---------------------------------------

    /// The scalar at the cursor, or `None` at end of input.
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// The scalar `n` positions ahead of the cursor.
    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    /// Advances the cursor by one scalar, maintaining line/column counters.
    /// Returns the consumed scalar, or `None` at end of input.
    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    /// Returns the substring of source between two cursor indices.
    fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }

    // ---- whitespace & comments ------------------------------------------

    /// Skips whitespace and `//` line comments. Returns `Some(doc)` if a `///`
    /// doc comment was encountered (the caller emits it as a token); returns
    /// `None` once the cursor sits on real token content or end of input.
    fn skip_trivia(&mut self) -> Option<Token> {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\n') => {
                    self.bump();
                }
                Some('\r') => {
                    // Treat both CRLF and a lone CR as whitespace. For a CRLF
                    // pair the '\n' is consumed on the next iteration, since it
                    // matches the whitespace arm above.
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    // Distinguish `///` (doc, exactly three slashes) from `//`
                    // and `////` (ordinary line comments, discarded).
                    let is_doc = self.peek_at(2) == Some('/') && self.peek_at(3) != Some('/');
                    if is_doc {
                        return Some(self.lex_doc_comment());
                    }
                    self.consume_line_comment();
                }
                _ => return None,
            }
        }
    }

    /// Consumes a `//` (or `////…`) line comment to end of line; produces no
    /// token.
    fn consume_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
    }

    /// Lexes a `///` doc comment (retained) to end of line.
    fn lex_doc_comment(&mut self) -> Token {
        let (line, col) = (self.line, self.col);
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
        Token::new(
            TokenKind::DocComment,
            self.slice(start, self.pos),
            line,
            col,
        )
    }

    // ---- public driver ---------------------------------------------------

    /// Produces the next token. After end of input is reached this returns an
    /// [`Eof`] token (repeatedly, if called again).
    ///
    /// [`Eof`]: TokenKind::Eof
    pub fn next_token(&mut self) -> Token {
        if let Some(doc) = self.skip_trivia() {
            return doc;
        }

        let (line, col) = (self.line, self.col);
        let start = self.pos;

        let c = match self.peek() {
            None => return Token::new(TokenKind::Eof, "", line, col),
            Some(c) => c,
        };

        // NUL anywhere in source is a lexical error (spec §1.2).
        if c == '\0' {
            self.bump();
            return Token::new(TokenKind::Error, "\0", line, col);
        }

        // `@` introduces either a builtin (`@name`) or an escaped identifier
        // (`@"..."`).
        if c == '@' {
            return self.lex_at(line, col);
        }

        // Identifiers and keywords.
        if is_ident_start(c) {
            return self.lex_ident_or_keyword(line, col);
        }

        // Numeric literals (int or float).
        if c.is_ascii_digit() {
            return self.lex_number(line, col);
        }

        // String, char, and multiline-string literals.
        match c {
            '"' => return self.lex_string(line, col),
            '\'' => return self.lex_char(line, col),
            '\\' if self.peek_at(1) == Some('\\') => {
                return self.lex_multiline_string(line, col);
            }
            _ => {}
        }

        // Operators and punctuation (maximal munch, spec §7.2).
        if let Some(tok) = self.lex_operator(line, col) {
            return tok;
        }

        // Anything else is an unexpected character.
        self.bump();
        Token::new(TokenKind::Error, self.slice(start, self.pos), line, col)
    }

    // ---- @builtins and escaped identifiers ------------------------------

    fn lex_at(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;
        self.bump(); // consume '@'
        match self.peek() {
            // Escaped identifier: @"..."
            Some('"') => {
                let str_tok = self.lex_string(line, col);
                if str_tok.kind == TokenKind::Error {
                    // Propagate the unterminated-string error, but report it as
                    // the escaped-identifier lexeme.
                    return Token::new(TokenKind::Error, self.slice(start, self.pos), line, col);
                }
                Token::new(
                    TokenKind::EscapedIdent,
                    self.slice(start, self.pos),
                    line,
                    col,
                )
            }
            // Builtin: @identifier
            Some(c) if is_ident_start(c) => {
                self.bump();
                while let Some(c) = self.peek() {
                    if is_ident_continue(c) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                Token::new(TokenKind::Builtin, self.slice(start, self.pos), line, col)
            }
            // A bare `@` is not a valid token.
            _ => Token::new(TokenKind::Error, self.slice(start, self.pos), line, col),
        }
    }

    // ---- identifiers & keywords -----------------------------------------

    fn lex_ident_or_keyword(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;
        self.bump();
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        let text = self.slice(start, self.pos);
        let kind = keyword_kind(&text).unwrap_or(TokenKind::Ident);
        Token::new(kind, text, line, col)
    }

    // ---- numeric literals ------------------------------------------------

    fn lex_number(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;

        // Radix-prefixed integers: 0x / 0o / 0b. Hex may also be a hex float.
        if self.peek() == Some('0') {
            match self.peek_at(1) {
                Some('x') | Some('X') => return self.lex_radix_or_hexfloat(line, col, start),
                Some('o') | Some('O') => {
                    self.bump();
                    self.bump();
                    self.consume_digits(|c| c.is_digit(8));
                    return self.finish_number(line, col, start, TokenKind::IntLiteral);
                }
                Some('b') | Some('B') => {
                    self.bump();
                    self.bump();
                    self.consume_digits(|c| c == '0' || c == '1');
                    return self.finish_number(line, col, start, TokenKind::IntLiteral);
                }
                _ => {}
            }
        }

        // Decimal integer part.
        self.consume_digits(|c| c.is_ascii_digit());

        let mut is_float = false;

        // Fractional part: requires a digit on BOTH sides of '.' (spec §6.2),
        // so `1.` and member access `x.0`-style do not turn into floats here.
        if self.peek() == Some('.') && self.peek_at(1).map(|c| c.is_ascii_digit()).unwrap_or(false)
        {
            is_float = true;
            self.bump(); // '.'
            self.consume_digits(|c| c.is_ascii_digit());
        }

        // Decimal exponent: e/E [sign] digits.
        if matches!(self.peek(), Some('e') | Some('E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.bump();
            }
            self.consume_digits(|c| c.is_ascii_digit());
        }

        let kind = if is_float {
            TokenKind::FloatLiteral
        } else {
            TokenKind::IntLiteral
        };
        self.finish_number(line, col, start, kind)
    }

    /// Handles `0x…` — either a hex integer or a hex float (which needs a
    /// `p`/`P` binary exponent per spec §6.2).
    fn lex_radix_or_hexfloat(&mut self, line: u32, col: u32, start: usize) -> Token {
        self.bump(); // '0'
        self.bump(); // 'x'/'X'
        self.consume_digits(|c| c.is_ascii_hexdigit());

        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.bump();
            self.consume_digits(|c| c.is_ascii_hexdigit());
        }
        if matches!(self.peek(), Some('p') | Some('P')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.bump();
            }
            self.consume_digits(|c| c.is_ascii_digit());
        }

        let kind = if is_float {
            TokenKind::FloatLiteral
        } else {
            TokenKind::IntLiteral
        };
        self.finish_number(line, col, start, kind)
    }

    /// Consumes a run of digits accepted by `accept`, allowing single `_`
    /// separators *between* digits (not leading/trailing/adjacent).
    fn consume_digits(&mut self, accept: impl Fn(char) -> bool) {
        while let Some(c) = self.peek() {
            if accept(c) {
                self.bump();
            } else if c == '_' && self.peek_at(1).map(&accept).unwrap_or(false) {
                self.bump(); // the separator
            } else {
                break;
            }
        }
    }

    fn finish_number(&mut self, line: u32, col: u32, start: usize, kind: TokenKind) -> Token {
        Token::new(kind, self.slice(start, self.pos), line, col)
    }

    // ---- string, char, multiline-string literals ------------------------

    fn lex_string(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;
        self.bump(); // opening '"'
        loop {
            match self.peek() {
                None | Some('\n') => {
                    // Unterminated string: a string literal may not span a line
                    // terminator (spec §6.4).
                    return Token::new(TokenKind::Error, self.slice(start, self.pos), line, col);
                }
                Some('"') => {
                    self.bump();
                    return Token::new(
                        TokenKind::StringLiteral,
                        self.slice(start, self.pos),
                        line,
                        col,
                    );
                }
                Some('\\') => {
                    // Consume the backslash and whatever escape char follows so
                    // an escaped quote does not terminate the string. Full
                    // escape validation happens in a later phase.
                    self.bump();
                    if self.peek().is_some() {
                        self.bump();
                    }
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn lex_char(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;
        self.bump(); // opening '\''
        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Token::new(TokenKind::Error, self.slice(start, self.pos), line, col);
                }
                Some('\'') => {
                    self.bump();
                    return Token::new(
                        TokenKind::CharLiteral,
                        self.slice(start, self.pos),
                        line,
                        col,
                    );
                }
                Some('\\') => {
                    self.bump();
                    if self.peek().is_some() {
                        self.bump();
                    }
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Lexes a multiline string: one or more consecutive `\\`-prefixed lines.
    /// The literal is raw and ends at the first line that does not begin (after
    /// optional leading whitespace) with `\\` (spec §6.5).
    fn lex_multiline_string(&mut self, line: u32, col: u32) -> Token {
        let start = self.pos;
        loop {
            // Consume the current `\\` line to end of line.
            self.bump(); // first '\'
            self.bump(); // second '\'
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                self.bump();
            }

            // Look ahead past the newline and leading whitespace for another
            // `\\` line. If absent, the literal ends here (newline excluded).
            let mut lookahead = self.pos;
            if self.chars.get(lookahead) == Some(&'\n') {
                lookahead += 1;
                while matches!(self.chars.get(lookahead), Some(' ') | Some('\t')) {
                    lookahead += 1;
                }
                if self.chars.get(lookahead) == Some(&'\\')
                    && self.chars.get(lookahead + 1) == Some(&'\\')
                {
                    // Advance the real cursor over the newline + whitespace and
                    // continue accumulating the next `\\` line.
                    while self.pos < lookahead {
                        self.bump();
                    }
                    continue;
                }
            }
            break;
        }
        Token::new(
            TokenKind::MultilineString,
            self.slice(start, self.pos),
            line,
            col,
        )
    }

    // ---- operators & punctuation (maximal munch) ------------------------

    /// Lexes one operator or punctuation token using maximal munch. Returns
    /// `None` if the cursor is not on a recognized symbolic character.
    fn lex_operator(&mut self, line: u32, col: u32) -> Option<Token> {
        let c0 = self.peek()?;
        let c1 = self.peek_at(1);
        let c2 = self.peek_at(2);

        // Helper to emit a fixed-width token and advance `n` scalars.
        let emit = |this: &mut Lexer, n: usize, kind: TokenKind| {
            let start = this.pos;
            for _ in 0..n {
                this.bump();
            }
            Some(Token::new(kind, this.slice(start, this.pos), line, col))
        };

        use TokenKind::*;
        match c0 {
            '+' => match c1 {
                Some('+') => emit(self, 2, PlusPlus),
                Some('=') => emit(self, 2, PlusEq),
                _ => emit(self, 1, Plus),
            },
            '-' => match c1 {
                Some('=') => emit(self, 2, MinusEq),
                // No `->` token in k2 (spec §7.1): a `>` after `-` is lexed as a
                // separate `Gt`, so `-` always yields `MinusEq` or `Minus` here.
                _ => emit(self, 1, Minus),
            },
            '*' => match c1 {
                Some('=') => emit(self, 2, StarEq),
                _ => emit(self, 1, Star),
            },
            '/' => match c1 {
                Some('=') => emit(self, 2, SlashEq),
                _ => emit(self, 1, Slash),
            },
            '%' => match c1 {
                Some('=') => emit(self, 2, PercentEq),
                _ => emit(self, 1, Percent),
            },
            '&' => match c1 {
                Some('=') => emit(self, 2, AmpEq),
                _ => emit(self, 1, Amp),
            },
            '|' => match c1 {
                Some('=') => emit(self, 2, PipeEq),
                _ => emit(self, 1, Pipe),
            },
            '^' => match c1 {
                Some('=') => emit(self, 2, CaretEq),
                _ => emit(self, 1, Caret),
            },
            '~' => emit(self, 1, Tilde),
            '<' => match (c1, c2) {
                (Some('<'), Some('=')) => emit(self, 3, ShlEq),
                (Some('<'), _) => emit(self, 2, Shl),
                (Some('='), _) => emit(self, 2, LtEq),
                _ => emit(self, 1, Lt),
            },
            '>' => match (c1, c2) {
                (Some('>'), Some('=')) => emit(self, 3, ShrEq),
                (Some('>'), _) => emit(self, 2, Shr),
                (Some('='), _) => emit(self, 2, GtEq),
                _ => emit(self, 1, Gt),
            },
            '=' => match c1 {
                Some('=') => emit(self, 2, EqEq),
                Some('>') => emit(self, 2, FatArrow),
                _ => emit(self, 1, Eq),
            },
            '!' => match c1 {
                // `!=` is the inequality operator; a bare `!` is the
                // error-union type constructor (never boolean negation).
                Some('=') => emit(self, 2, NotEq),
                _ => emit(self, 1, Bang),
            },
            '?' => emit(self, 1, Question),
            '.' => match (c1, c2) {
                (Some('.'), Some('.')) => emit(self, 3, DotDotDot),
                (Some('.'), _) => emit(self, 2, DotDot),
                (Some('*'), _) => emit(self, 2, DotStar),
                (Some('?'), _) => emit(self, 2, DotQuestion),
                _ => emit(self, 1, Dot),
            },
            '(' => emit(self, 1, LParen),
            ')' => emit(self, 1, RParen),
            '{' => emit(self, 1, LBrace),
            '}' => emit(self, 1, RBrace),
            '[' => emit(self, 1, LBracket),
            ']' => emit(self, 1, RBracket),
            ',' => emit(self, 1, Comma),
            ';' => emit(self, 1, Semicolon),
            ':' => emit(self, 1, Colon),
            _ => None,
        }
    }
}

impl Iterator for Lexer {
    type Item = Token;

    /// Yields tokens up to and including the final [`Eof`], then `None`.
    ///
    /// [`Eof`]: TokenKind::Eof
    fn next(&mut self) -> Option<Token> {
        if self.done {
            return None;
        }
        let tok = self.next_token();
        if tok.kind == TokenKind::Eof {
            self.done = true;
        }
        Some(tok)
    }
}

/// `true` if `c` may begin an identifier: an ASCII letter or `_` (spec §4).
fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

/// `true` if `c` may continue an identifier: a letter, digit, or `_`.
fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Tokenizes a complete `.k2` source string into a `Vec<Token>`, ending with a
/// single [`TokenKind::Eof`] token. This is the one-call convenience entry
/// point most callers want.
///
/// ```
/// use k2_lexer::{tokenize, TokenKind};
/// let toks = tokenize("pub fn main() !void {}");
/// assert_eq!(toks.first().unwrap().kind, TokenKind::KwPub);
/// assert_eq!(toks.last().unwrap().kind, TokenKind::Eof);
/// ```
pub fn tokenize(src: &str) -> Vec<Token> {
    Lexer::new(src).collect()
}

// =========================================================================
//  Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects just the token kinds (dropping the trailing `Eof`) for terse
    /// assertions.
    fn kinds(src: &str) -> Vec<TokenKind> {
        let mut v: Vec<TokenKind> = tokenize(src).into_iter().map(|t| t.kind).collect();
        assert_eq!(v.last(), Some(&TokenKind::Eof), "stream must end with Eof");
        v.pop();
        v
    }

    #[test]
    fn empty_source_is_just_eof() {
        assert_eq!(tokenize("").len(), 1);
        assert_eq!(tokenize("   \n\t  ").len(), 1);
        assert_eq!(tokenize("").first().unwrap().kind, TokenKind::Eof);
    }

    #[test]
    fn tokenize_const_decl() {
        use TokenKind::*;
        assert_eq!(
            kinds("const x = 1;"),
            vec![KwConst, Ident, Eq, IntLiteral, Semicolon]
        );
    }

    #[test]
    fn tokenize_all_keywords() {
        // Every one of the locked keyword set must lex to its keyword kind,
        // not to `Ident`. The charter's `keywords` list has 35 entries.
        let src = "const var pub fn comptime return struct enum union error \
                   if else while for switch break continue defer errdefer try \
                   catch orelse and or not unreachable test extern export \
                   inline align true false null undefined";
        let ks = kinds(src);
        assert_eq!(ks.len(), 35, "there are 35 whitespace-separated keywords");
        assert!(ks.iter().all(|k| k.is_keyword()), "all lex as keywords");
    }

    #[test]
    fn predeclared_type_names_are_idents_not_keywords() {
        // Primitive type names are predeclared identifiers, NOT keywords
        // (spec §5.3).
        use TokenKind::*;
        assert_eq!(
            kinds("i32 usize f64 bool void type anyerror comptime_int"),
            vec![Ident, Ident, Ident, Ident, Ident, Ident, Ident, Ident]
        );
    }

    #[test]
    fn builtins_and_escaped_identifiers() {
        use TokenKind::*;
        // @import("std")
        assert_eq!(
            kinds("@import(\"std\")"),
            vec![Builtin, LParen, StringLiteral, RParen]
        );
        // @sizeOf and @"escaped"
        let toks = tokenize("@sizeOf @\"const\"");
        assert_eq!(toks[0].kind, Builtin);
        assert_eq!(toks[0].text, "@sizeOf");
        assert_eq!(toks[1].kind, EscapedIdent);
        assert_eq!(toks[1].text, "@\"const\"");
    }

    #[test]
    fn line_comments_are_discarded_doc_comments_retained() {
        use TokenKind::*;
        // A `//` comment produces no token; `///` is retained; `////` is an
        // ordinary comment again.
        let toks = tokenize(
            "// dropped\n\
             /// kept\n\
             //// also dropped\n\
             const a = 1;",
        );
        let ks: Vec<_> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            ks,
            vec![DocComment, KwConst, Ident, Eq, IntLiteral, Semicolon, Eof]
        );
        assert_eq!(toks[0].text, "/// kept");
    }

    #[test]
    fn integer_literal_bases_and_separators() {
        use TokenKind::*;
        assert_eq!(kinds("1_000_000"), vec![IntLiteral]);
        assert_eq!(kinds("0xFF_FF"), vec![IntLiteral]);
        assert_eq!(kinds("0o755"), vec![IntLiteral]);
        assert_eq!(kinds("0b1010_0101"), vec![IntLiteral]);

        let toks = tokenize("0xFF_FF");
        assert_eq!(toks[0].text, "0xFF_FF");
    }

    #[test]
    fn float_literals_decimal_and_hex() {
        use TokenKind::*;
        assert_eq!(kinds("3.141_592"), vec![FloatLiteral]);
        assert_eq!(kinds("6.022e23"), vec![FloatLiteral]);
        assert_eq!(kinds("1.5e-9"), vec![FloatLiteral]);
        assert_eq!(kinds("0x1.8p3"), vec![FloatLiteral]);
    }

    #[test]
    fn trailing_dot_is_not_a_float() {
        // `1.` is not a valid float; the lexer keeps `1` an int and `.` a
        // separate operator (spec §6.2). This keeps `x.0` field access
        // unambiguous.
        use TokenKind::*;
        assert_eq!(kinds("1.foo"), vec![IntLiteral, Dot, Ident]);
    }

    #[test]
    fn string_and_char_literals() {
        use TokenKind::*;
        assert_eq!(kinds("\"Hello, k2!\\n\""), vec![StringLiteral]);
        // Escaped quote inside a string does not terminate it.
        assert_eq!(kinds("\"a\\\"b\""), vec![StringLiteral]);
        assert_eq!(kinds("'a'"), vec![CharLiteral]);
        assert_eq!(kinds("'\\n'"), vec![CharLiteral]);
        assert_eq!(kinds("'\\''"), vec![CharLiteral]);
    }

    #[test]
    fn unterminated_string_is_an_error_token() {
        use TokenKind::*;
        // The newline ends the (unterminated) string as an error; the rest
        // still lexes, demonstrating recovery.
        let ks = kinds("\"oops\nconst");
        assert_eq!(ks, vec![Error, KwConst]);
    }

    #[test]
    fn multiline_string_spans_backslash_lines() {
        use TokenKind::*;
        let src = "const usage =\n\
                   \x20\x20\x20\x20\\\\Usage: k2 build\n\
                   \x20\x20\x20\x20\\\\  --target <triple>\n\
                   \x20\x20\x20\x20;";
        let ks = kinds(src);
        assert_eq!(ks, vec![KwConst, Ident, Eq, MultilineString, Semicolon]);
        let toks = tokenize(src);
        let ml = toks.iter().find(|t| t.kind == MultilineString).unwrap();
        assert!(ml.text.contains("Usage: k2 build"));
        assert!(ml.text.contains("--target <triple>"));
        // The literal does not swallow the closing `;` line.
        assert!(!ml.text.contains(';'));
    }

    #[test]
    fn operators_maximal_munch() {
        use TokenKind::*;
        // `<<=` is one token, not `<<` then `=` (spec §7.2).
        assert_eq!(kinds("<<="), vec![ShlEq]);
        assert_eq!(kinds(">>="), vec![ShrEq]);
        assert_eq!(kinds("=="), vec![EqEq]);
        assert_eq!(kinds("!="), vec![NotEq]);
        assert_eq!(kinds("++"), vec![PlusPlus]);
        assert_eq!(kinds("=>"), vec![FatArrow]);
        // The `.`-prefixed cluster.
        assert_eq!(kinds(".*"), vec![DotStar]);
        assert_eq!(kinds(".?"), vec![DotQuestion]);
        assert_eq!(kinds("..."), vec![DotDotDot]);
        assert_eq!(kinds(".."), vec![DotDot]);
        assert_eq!(kinds("."), vec![Dot]);
    }

    #[test]
    fn no_arrow_token_minus_then_gt() {
        // Per spec §7.1, k2 has no `->` token. `->` lexes as `Minus` then `Gt`,
        // and `-=` / `=>` are unaffected.
        use TokenKind::*;
        assert_eq!(kinds("->"), vec![Minus, Gt]);
        assert_eq!(kinds("-="), vec![MinusEq]);
        assert_eq!(kinds("a - b"), vec![Ident, Minus, Ident]);
    }

    #[test]
    fn bang_is_error_union_not_boolean_not() {
        // `!void` is an error-union type; `!` lexes as a standalone `Bang`,
        // and word operators `and`/`or`/`not` are keywords (charter rule).
        use TokenKind::*;
        assert_eq!(kinds("!void"), vec![Bang, Ident]);
        assert_eq!(
            kinds("a and not b or c"),
            vec![Ident, KwAnd, KwNot, Ident, KwOr, Ident]
        );
    }

    #[test]
    fn line_and_column_tracking() {
        // Columns are 1-based and reset on each newline.
        let toks = tokenize("const x\n  = 1;");
        let by_text = |s: &str| toks.iter().find(|t| t.text == s).unwrap().clone();
        let kw = by_text("const");
        assert_eq!((kw.line, kw.col), (1, 1));
        let x = by_text("x");
        assert_eq!((x.line, x.col), (1, 7));
        let eq = by_text("=");
        assert_eq!((eq.line, eq.col), (2, 3));
    }

    #[test]
    fn hello_world_snippet_lexes_cleanly() {
        // The canonical "Hello, k2!" program (charter snippet) must contain no
        // Error tokens.
        let src = "const std = @import(\"std\");\n\
                   \n\
                   pub fn main(sys: *System) !void {\n\
                   \x20\x20\x20\x20const out = sys.io.stdout();\n\
                   \x20\x20\x20\x20try out.print(\"Hello, k2!\\n\", .{});\n\
                   }";
        let toks = tokenize(src);
        assert!(
            toks.iter().all(|t| t.kind != TokenKind::Error),
            "no lexical errors in the hello-world program: {:?}",
            toks.iter()
                .filter(|t| t.kind == TokenKind::Error)
                .collect::<Vec<_>>()
        );
        // Spot-check a few salient tokens.
        let ks: Vec<_> = toks.iter().map(|t| t.kind).collect();
        assert!(ks.contains(&TokenKind::Builtin)); // @import
        assert!(ks.contains(&TokenKind::KwPub));
        assert!(ks.contains(&TokenKind::KwFn));
        assert!(ks.contains(&TokenKind::Bang)); // the `!` of `!void`
        assert!(!ks.contains(&TokenKind::DotStar)); // none in this src
    }

    #[test]
    fn keyword_kind_round_trips() {
        // The locked charter keyword set, in charter order. Each must map to a
        // distinct keyword `TokenKind` via `keyword_kind`, and every result
        // must satisfy `is_keyword`.
        let words = [
            "const",
            "var",
            "pub",
            "fn",
            "comptime",
            "return",
            "struct",
            "enum",
            "union",
            "error",
            "if",
            "else",
            "while",
            "for",
            "switch",
            "break",
            "continue",
            "defer",
            "errdefer",
            "try",
            "catch",
            "orelse",
            "and",
            "or",
            "not",
            "unreachable",
            "test",
            "extern",
            "export",
            "inline",
            "align",
            "true",
            "false",
            "null",
            "undefined",
        ];
        assert_eq!(
            words.len(),
            35,
            "the locked charter keyword set has 35 entries"
        );
        let mut seen = std::collections::HashSet::new();
        for w in words {
            let k = keyword_kind(w).unwrap_or_else(|| panic!("`{w}` should be a keyword"));
            assert!(k.is_keyword());
            seen.insert(k);
        }
        assert_eq!(seen.len(), 35, "each keyword maps to a distinct kind");
    }
}
