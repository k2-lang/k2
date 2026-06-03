//! # k2-syntax — abstract syntax tree for the k2 programming language
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This crate defines a small but real AST for k2: source [`Span`]s, top-level
//! [`Item`]s, [`Stmt`]ements, and [`Expr`]essions. It mirrors the shapes in
//! `docs/grammar.ebnf` while staying deliberately minimal — just enough nodes
//! to represent the canonical charter snippets (a `const std = @import("std");`
//! declaration, a `pub fn main(...) !void { ... }` function, `try`/`catch`
//! error handling, and so on).
//!
//! The parser that produces these nodes is a separate, later stage; this crate
//! only owns the data definitions plus a handful of constructor helpers, so the
//! lexer, a future parser, and tooling all agree on one representation.
//!
//! It depends on [`k2_lexer`] purely for the [`Token`](k2_lexer::Token) type
//! used to build a [`Span`] from a lexed token.

use k2_lexer::Token;

/// A half-open source range, used to attach every AST node to the bytes it came
/// from for diagnostics. Offsets are scalar indices into the source; `line` and
/// `col` (1-based) record the start position for human-readable messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Span {
    /// Inclusive start offset (scalar index).
    pub start: u32,
    /// Exclusive end offset (scalar index).
    pub end: u32,
    /// 1-based line of `start`.
    pub line: u32,
    /// 1-based column of `start`.
    pub col: u32,
}

impl Span {
    /// Builds a span from explicit fields.
    pub fn new(start: u32, end: u32, line: u32, col: u32) -> Span {
        Span {
            start,
            end,
            line,
            col,
        }
    }

    /// Builds a zero-width span anchored at a single offset/position. Useful for
    /// synthesized nodes and for marking the point of a parse error.
    pub fn point(offset: u32, line: u32, col: u32) -> Span {
        Span::new(offset, offset, line, col)
    }

    /// Builds a span covering a lexer [`Token`], given the token's `start`
    /// offset. The lexer records `line`/`col` and the token's `text` gives its
    /// length, so the end offset is `start + len`.
    pub fn of_token(tok: &Token, start: u32) -> Span {
        let len = tok.text.chars().count() as u32;
        Span::new(start, start + len, tok.line, tok.col)
    }

    /// The smallest span enclosing both `self` and `other`. The resulting
    /// `line`/`col` are taken from whichever span starts earlier.
    pub fn merge(self, other: Span) -> Span {
        let (line, col) = if self.start <= other.start {
            (self.line, self.col)
        } else {
            (other.line, other.col)
        };
        Span::new(
            self.start.min(other.start),
            self.end.max(other.end),
            line,
            col,
        )
    }

    /// The number of scalars this span covers.
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// `true` if this span is zero-width.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Integer literal radix, recorded so later phases can validate digits and
/// reproduce the source form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntBase {
    /// Plain decimal, e.g. `1_000`.
    Dec,
    /// `0x…` hexadecimal.
    Hex,
    /// `0o…` octal.
    Oct,
    /// `0b…` binary.
    Bin,
}

/// A binary operator. k2 has no operator overloading (per *no hidden control
/// flow*), so this set is fixed and total. Note the word-based boolean
/// operators `and`/`or` and the absence of any boolean `!`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,    // +
    Sub,    // -
    Mul,    // *
    Div,    // /
    Rem,    // %
    Concat, // ++  (comptime array/string concatenation)
    BitAnd, // &
    BitOr,  // |
    BitXor, // ^
    Shl,    // <<
    Shr,    // >>
    Eq,     // ==
    Ne,     // !=
    Lt,     // <
    Le,     // <=
    Gt,     // >
    Ge,     // >=
    And,    // and  (short-circuit, keyword)
    Or,     // or   (short-circuit, keyword)
}

/// A prefix/unary operator (precedence level 2 in the spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,    // -
    BitNot, // ~
    Not,    // not  (boolean negation, keyword)
    AddrOf, // &    (address-of)
    Try,    // try  (error propagation)
}

/// An expression node. This is the value-producing core of the grammar; the
/// variants here cover what the canonical snippets need without yet modelling
/// every control-flow-in-expression form.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// An integer literal with its radix, e.g. `0xFF_FF`. `text` is the verbatim
    /// lexeme (digit separators retained).
    Int {
        text: String,
        base: IntBase,
        span: Span,
    },
    /// A float literal, e.g. `6.022e23`.
    Float { text: String, span: Span },
    /// A string literal, including its surrounding quotes in `text`.
    Str { text: String, span: Span },
    /// A character literal, e.g. `'a'`.
    Char { text: String, span: Span },
    /// One of the literal keywords `true` / `false`.
    Bool { value: bool, span: Span },
    /// The optional-absence literal `null`.
    Null { span: Span },
    /// The uninitialized placeholder `undefined`.
    Undefined { span: Span },
    /// A bare identifier reference (variable, function, or type name).
    Ident { name: String, span: Span },
    /// A compile-time builtin call, e.g. `@import("std")` or `@sizeOf(u32)`.
    /// `name` includes the leading `@`.
    Builtin {
        name: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// Field/member access `obj.field`.
    Field {
        base: Box<Expr>,
        field: String,
        span: Span,
    },
    /// A function/method call `callee(args...)`.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    /// A binary operation `lhs op rhs`.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// A prefix unary operation `op operand`.
    Unary {
        op: UnOp,
        operand: Box<Expr>,
        span: Span,
    },
}

impl Expr {
    /// The source span of this expression, regardless of variant.
    pub fn span(&self) -> Span {
        match self {
            Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Str { span, .. }
            | Expr::Char { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Null { span }
            | Expr::Undefined { span }
            | Expr::Ident { span, .. }
            | Expr::Builtin { span, .. }
            | Expr::Field { span, .. }
            | Expr::Call { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. } => *span,
        }
    }

    /// Constructs an identifier-reference expression.
    pub fn ident(name: impl Into<String>, span: Span) -> Expr {
        Expr::Ident {
            name: name.into(),
            span,
        }
    }

    /// Constructs a binary-operation expression, deriving its span from the
    /// operands.
    pub fn binary(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
        let span = lhs.span().merge(rhs.span());
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        }
    }
}

/// A statement node — the body of a block. k2's only non-linear control flow is
/// the explicit keyword set; this minimal subset models declarations, the most
/// common cleanup/return forms, and bare expression statements.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `const name [: type] = value;` — an immutable binding.
    Const {
        name: String,
        ty: Option<Expr>,
        value: Expr,
        span: Span,
    },
    /// `var name [: type] [= value];` — a mutable binding.
    Var {
        name: String,
        ty: Option<Expr>,
        value: Option<Expr>,
        span: Span,
    },
    /// `defer <stmt-or-expr>;` — cleanup that runs on any scope exit.
    Defer { body: Box<Stmt>, span: Span },
    /// `errdefer <stmt-or-expr>;` — cleanup that runs only on the error path.
    Errdefer { body: Box<Stmt>, span: Span },
    /// `return [expr];`
    Return { value: Option<Expr>, span: Span },
    /// A bare expression used for its effect, e.g. `try out.print(...);`.
    Expr { expr: Expr, span: Span },
}

impl Stmt {
    /// The source span of this statement.
    pub fn span(&self) -> Span {
        match self {
            Stmt::Const { span, .. }
            | Stmt::Var { span, .. }
            | Stmt::Defer { span, .. }
            | Stmt::Errdefer { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Expr { span, .. } => *span,
        }
    }

    /// Constructs a `const` declaration statement.
    pub fn const_decl(name: impl Into<String>, ty: Option<Expr>, value: Expr, span: Span) -> Stmt {
        Stmt::Const {
            name: name.into(),
            ty,
            value,
            span,
        }
    }
}

/// A function parameter, e.g. `sys: *System` or `comptime T: type`. The type is
/// represented as an [`Expr`] because k2 types are ordinary expressions parsed
/// by the type grammar (a postfix-modifier chain over a primary).
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    /// `true` if the parameter is `comptime`-qualified.
    pub is_comptime: bool,
    /// The parameter name (`_` for a discard).
    pub name: String,
    /// The parameter's type expression.
    pub ty: Expr,
    /// Source span of the whole parameter.
    pub span: Span,
}

/// A top-level item. At file scope k2 admits only declarations and tests (there
/// are no free statements at the top level — see the grammar's
/// `top_level_decl`).
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A top-level `const` declaration, e.g. `const std = @import("std");`.
    /// `is_pub` records a leading `pub`.
    Const {
        is_pub: bool,
        name: String,
        ty: Option<Expr>,
        value: Expr,
        span: Span,
    },
    /// A top-level `var` declaration.
    Var {
        is_pub: bool,
        name: String,
        ty: Option<Expr>,
        value: Option<Expr>,
        span: Span,
    },
    /// A function declaration, e.g. `pub fn main(sys: *System) !void { ... }`.
    /// `ret` is the return-type expression (which may be an error union such as
    /// `!void`); `body` is `None` for an `extern` prototype.
    Fn {
        is_pub: bool,
        name: String,
        params: Vec<Param>,
        ret: Expr,
        body: Option<Vec<Stmt>>,
        span: Span,
    },
    /// A `test "name" { ... }` declaration.
    Test {
        name: Option<String>,
        body: Vec<Stmt>,
        span: Span,
    },
}

impl Item {
    /// The source span of this item.
    pub fn span(&self) -> Span {
        match self {
            Item::Const { span, .. }
            | Item::Var { span, .. }
            | Item::Fn { span, .. }
            | Item::Test { span, .. } => *span,
        }
    }

    /// Constructs a top-level `const` item.
    pub fn const_item(
        is_pub: bool,
        name: impl Into<String>,
        ty: Option<Expr>,
        value: Expr,
        span: Span,
    ) -> Item {
        Item::Const {
            is_pub,
            name: name.into(),
            ty,
            value,
            span,
        }
    }
}

/// A whole parsed source file: an ordered list of top-level [`Item`]s. (Doc
/// comments and module-level metadata can be layered on later; this keeps the
/// root node minimal.)
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SourceFile {
    /// The file's top-level items in source order.
    pub items: Vec<Item>,
}

// =========================================================================
//  Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use k2_lexer::{tokenize, TokenKind};

    #[test]
    fn span_merge_and_helpers() {
        let a = Span::new(0, 5, 1, 1);
        let b = Span::new(10, 14, 2, 3);
        let m = a.merge(b);
        assert_eq!(m.start, 0);
        assert_eq!(m.end, 14);
        // The merged line/col come from the earlier-starting span `a`.
        assert_eq!((m.line, m.col), (1, 1));
        assert_eq!(m.len(), 14);
        assert!(!m.is_empty());
        assert!(Span::point(7, 1, 8).is_empty());
    }

    #[test]
    fn span_from_token_uses_text_length() {
        // `const` is 5 scalars wide; a span anchored at offset 0 ends at 5.
        let toks = tokenize("const");
        let kw = &toks[0];
        assert_eq!(kw.kind, TokenKind::KwConst);
        let span = Span::of_token(kw, 0);
        assert_eq!(span.len(), 5);
        assert_eq!((span.line, span.col), (1, 1));
    }

    #[test]
    fn build_const_std_import_item() {
        // Hand-build the AST for the canonical first line of every k2 program:
        //     const std = @import("std");
        let value = Expr::Builtin {
            name: "@import".into(),
            args: vec![Expr::Str {
                text: "\"std\"".into(),
                span: Span::new(19, 24, 1, 20),
            }],
            span: Span::new(12, 25, 1, 13),
        };
        let item = Item::const_item(false, "std", None, value, Span::new(0, 27, 1, 1));

        match &item {
            Item::Const {
                is_pub,
                name,
                value,
                ..
            } => {
                assert!(!is_pub);
                assert_eq!(name, "std");
                assert!(matches!(value, Expr::Builtin { name, .. } if name == "@import"));
            }
            _ => panic!("expected a const item"),
        }
        assert_eq!(item.span().start, 0);
    }

    #[test]
    fn build_binary_expression_derives_span() {
        // `a + b` with `a` at [0,1) and `b` at [4,5).
        let lhs = Expr::ident("a", Span::new(0, 1, 1, 1));
        let rhs = Expr::ident("b", Span::new(4, 5, 1, 5));
        let sum = Expr::binary(BinOp::Add, lhs, rhs);
        assert_eq!(sum.span(), Span::new(0, 5, 1, 1));
        match sum {
            Expr::Binary { op, .. } => assert_eq!(op, BinOp::Add),
            _ => panic!("expected a binary expression"),
        }
    }

    #[test]
    fn build_main_fn_item() {
        // pub fn main(sys: *System) !void { return; }
        let sys_ty = Expr::Unary {
            op: UnOp::AddrOf, // stand-in for the `*` pointer modifier on `System`
            operand: Box::new(Expr::ident("System", Span::new(22, 28, 1, 23))),
            span: Span::new(21, 28, 1, 22),
        };
        let param = Param {
            is_comptime: false,
            name: "sys".into(),
            ty: sys_ty,
            span: Span::new(16, 28, 1, 17),
        };
        let ret = Expr::Unary {
            op: UnOp::Not, // stand-in marker for the `!` of the `!void` error union
            operand: Box::new(Expr::ident("void", Span::new(31, 35, 1, 32))),
            span: Span::new(30, 35, 1, 31),
        };
        let body = vec![Stmt::Return {
            value: None,
            span: Span::new(38, 45, 1, 39),
        }];
        let item = Item::Fn {
            is_pub: true,
            name: "main".into(),
            params: vec![param],
            ret,
            body: Some(body),
            span: Span::new(0, 47, 1, 1),
        };

        match item {
            Item::Fn {
                is_pub,
                name,
                params,
                body,
                ..
            } => {
                assert!(is_pub);
                assert_eq!(name, "main");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "sys");
                assert_eq!(body.unwrap().len(), 1);
            }
            _ => panic!("expected a fn item"),
        }
    }

    #[test]
    fn source_file_collects_items() {
        let mut file = SourceFile::default();
        file.items.push(Item::const_item(
            false,
            "x",
            None,
            Expr::Int {
                text: "1".into(),
                base: IntBase::Dec,
                span: Span::new(10, 11, 1, 11),
            },
            Span::new(0, 12, 1, 1),
        ));
        assert_eq!(file.items.len(), 1);
    }
}
