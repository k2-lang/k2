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
    Orelse, // orelse  (null-coalescing, level 12, left-assoc)
    /// `||` — error-set union. The lexer emits two adjacent `Pipe` tokens (there
    /// is no `PipePipe`); the parser recognizes the merge by their adjacency.
    /// See the grammar-vs-example reconciliation notes in the parser crate.
    ErrSetMerge,
}

/// A prefix/unary operator (precedence level 2 in the spec).
///
/// Note that prefix `comptime` is *not* in this set: it is modelled by the
/// dedicated [`Expr::Comptime`] node so the S-expression printer can label it
/// distinctly. The pointer/optional/slice/array *type* prefixes are likewise
/// their own [`Expr`] variants ([`Expr::Pointer`], [`Expr::Optional`], …),
/// since k2 types are ordinary expressions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,    // -
    BitNot, // ~
    Not,    // not  (boolean negation, keyword)
    AddrOf, // &    (address-of)
    Try,    // try  (error propagation)
}

/// An assignment operator. Assignment is a *statement* in k2 (never an
/// expression — see [`Stmt::Assign`]); this enum names the compound forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Eq,    // =
    AddEq, // +=
    SubEq, // -=
    MulEq, // *=
    DivEq, // /=
    RemEq, // %=
    AndEq, // &=
    OrEq,  // |=
    XorEq, // ^=
    ShlEq, // <<=
    ShrEq, // >>=
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

    // ---- Type constructors (types are expressions) -----------------------
    /// `?T` — optional type. Written prefix; modifies the inner type.
    Optional { inner: Box<Expr>, span: Span },
    /// `*T`, `*const T`, `*align(e) T` — single-item pointer type.
    Pointer {
        is_const: bool,
        align: Option<Box<Expr>>,
        inner: Box<Expr>,
        span: Span,
    },
    /// `[]T`, `[]const T`, `[]align(e) T` — slice type.
    Slice {
        is_const: bool,
        align: Option<Box<Expr>>,
        inner: Box<Expr>,
        span: Span,
    },
    /// `[N]T` — array type. `len` may be `_` (inferred length).
    ArrayType {
        len: Box<Expr>,
        inner: Box<Expr>,
        span: Span,
    },
    /// `E!T` (explicit) or `!T` (inferred, `err == None`) — error-union type.
    ErrorUnion {
        err: Option<Box<Expr>>,
        ok: Box<Expr>,
        span: Span,
    },
    /// `fn (params) Ret` — function (pointer) type.
    FnType {
        params: Vec<Param>,
        is_varargs: bool,
        ret: Box<Expr>,
        span: Span,
    },
    /// `error { A, B, C }` — error-set type.
    ErrorSet { fields: Vec<String>, span: Span },
    /// `anytype` — predeclared inferred-parameter type marker.
    AnyType { span: Span },
    /// `struct {...}` / `enum(tag?) {...}` / `union(enum|type)? {...}` — a
    /// container *type* expression (the value of `const T = struct {...};`).
    /// Boxed so it does not bloat the [`Expr`] enum.
    Container(Box<Container>),

    // ---- Literals & postfix (value side) ---------------------------------
    /// `.Name` — enum literal in inferred context.
    EnumLiteral { name: String, span: Span },
    /// `error.Name` — a specific error *value*.
    ErrorLiteral { name: String, span: Span },
    /// `.{ ... }` (`ty == None`) or typed `T{ ... }` (`ty == Some`) initializer.
    Init {
        ty: Option<Box<Expr>>,
        body: InitBody,
        span: Span,
    },
    /// `base[index]`.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// `base[lo..hi]` (with `hi` optional, i.e. `base[lo..]`).
    SliceExpr {
        base: Box<Expr>,
        lo: Box<Expr>,
        hi: Option<Box<Expr>>,
        span: Span,
    },
    /// `base.*` — pointer dereference (postfix).
    Deref { base: Box<Expr>, span: Span },
    /// `base.?` — optional unwrap (postfix).
    Unwrap { base: Box<Expr>, span: Span },
    /// `lhs catch [|err|] rhs`. `catch` carries an optional capture, so unlike
    /// `orelse` it is its own node rather than a [`BinOp`].
    Catch {
        lhs: Box<Expr>,
        capture: Option<String>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `comptime EXPR` in expression position.
    Comptime { inner: Box<Expr>, span: Span },
    /// `unreachable`.
    Unreachable { span: Span },

    // ---- Control flow in expression position -----------------------------
    /// A labeled or bare block used as an expression: `lbl: { stmts }`.
    Block {
        label: Option<String>,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `if (cond) [|cap|] then [else [|cap|] otherwise]`. In the expression
    /// form `else_branch` is `Some`; the statement form may leave it `None`.
    If {
        cond: Box<Expr>,
        capture: Option<Capture>,
        then_branch: Box<Expr>,
        else_capture: Option<Capture>,
        else_branch: Option<Box<Expr>>,
        span: Span,
    },
    /// `[lbl:] [inline] while (cond) [|cap|] [: (cont)] body [else [|cap|] e]`.
    While {
        label: Option<String>,
        is_inline: bool,
        cond: Box<Expr>,
        capture: Option<Capture>,
        cont: Option<Box<Stmt>>,
        body: Box<Expr>,
        else_capture: Option<Capture>,
        else_branch: Option<Box<Expr>>,
        span: Span,
    },
    /// `[lbl:] [inline] for (operands) |captures| body [else e]`.
    For {
        label: Option<String>,
        is_inline: bool,
        operands: Vec<ForOperand>,
        captures: Vec<CaptureName>,
        body: Box<Expr>,
        else_branch: Option<Box<Expr>>,
        span: Span,
    },
    /// `switch (scrutinee) { arms }`.
    Switch {
        scrutinee: Box<Expr>,
        arms: Vec<SwitchArm>,
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
            | Expr::Unary { span, .. }
            | Expr::Optional { span, .. }
            | Expr::Pointer { span, .. }
            | Expr::Slice { span, .. }
            | Expr::ArrayType { span, .. }
            | Expr::ErrorUnion { span, .. }
            | Expr::FnType { span, .. }
            | Expr::ErrorSet { span, .. }
            | Expr::AnyType { span }
            | Expr::EnumLiteral { span, .. }
            | Expr::ErrorLiteral { span, .. }
            | Expr::Init { span, .. }
            | Expr::Index { span, .. }
            | Expr::SliceExpr { span, .. }
            | Expr::Deref { span, .. }
            | Expr::Unwrap { span, .. }
            | Expr::Catch { span, .. }
            | Expr::Comptime { span, .. }
            | Expr::Unreachable { span }
            | Expr::Block { span, .. }
            | Expr::If { span, .. }
            | Expr::While { span, .. }
            | Expr::For { span, .. }
            | Expr::Switch { span, .. } => *span,
            Expr::Container(c) => c.span,
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

/// The body of an initializer literal (`.{ ... }` or `T{ ... }`): either named
/// fields or a positional tuple.
#[derive(Clone, Debug, PartialEq)]
pub enum InitBody {
    /// `.f = v, .g = w` (possibly empty for `.{}`).
    Fields(Vec<FieldInit>),
    /// `e, e, e` — positional/tuple form.
    Tuple(Vec<Expr>),
}

/// One `.name = value` entry in a named initializer body.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldInit {
    /// The field name (without the leading `.`).
    pub name: String,
    /// The value expression.
    pub value: Expr,
    /// Source span of the whole `.name = value` entry.
    pub span: Span,
}

/// An `|x|` / `|*x|` / `|a, b|` payload capture for `if`/`while`/`switch` and
/// their `else` clauses.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    /// The captured names, in order.
    pub names: Vec<CaptureName>,
    /// Source span of the whole `| ... |` clause.
    pub span: Span,
}

/// A single capture name, possibly by-pointer (`*name`). `name == "_"` is a
/// discard.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureName {
    /// `true` for a by-pointer capture (`|*x|` / `for (...) |*slot|`).
    pub by_ref: bool,
    /// The captured identifier (or `_` to discard).
    pub name: String,
    /// Source span of this capture name (including a leading `*`).
    pub span: Span,
}

/// A `for` operand: a value to iterate, or an index range `lo..[hi]`.
#[derive(Clone, Debug, PartialEq)]
pub enum ForOperand {
    /// A slice/array value to iterate.
    Value(Expr),
    /// An index range `lo..` or `lo..hi`.
    Range {
        lo: Box<Expr>,
        hi: Option<Box<Expr>>,
        span: Span,
    },
}

impl ForOperand {
    /// The source span of this operand.
    pub fn span(&self) -> Span {
        match self {
            ForOperand::Value(e) => e.span(),
            ForOperand::Range { span, .. } => *span,
        }
    }
}

/// One arm of a `switch`: `pattern => [|cap|] body,`.
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchArm {
    /// The pattern matched by this arm.
    pub pattern: SwitchPattern,
    /// An optional payload capture (`|err|`, `|*v|`).
    pub capture: Option<Capture>,
    /// The arm body (a block or a single expression).
    pub body: Expr,
    /// Source span of the whole arm.
    pub span: Span,
}

/// A `switch` arm pattern: `else`, or a list of items (each possibly a range).
#[derive(Clone, Debug, PartialEq)]
pub enum SwitchPattern {
    /// The catch-all `else` pattern.
    Else,
    /// One or more comma-separated items.
    Items(Vec<SwitchItem>),
}

/// A single `switch` item: an expression, or an inclusive range `lo ... hi`.
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchItem {
    /// The low bound (or the sole value when `hi` is `None`).
    pub lo: Expr,
    /// The high bound of an inclusive range, or `None`.
    pub hi: Option<Expr>,
    /// Source span of this item.
    pub span: Span,
}

/// A container *type* — the value of a `const T = struct/enum/union {...};`.
#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    /// Which container kind (with its tag/extern data).
    pub kind: ContainerKind,
    /// The members in source order.
    pub members: Vec<Member>,
    /// Source span of the whole container.
    pub span: Span,
}

/// The kind of a container, with its kind-specific data.
#[derive(Clone, Debug, PartialEq)]
pub enum ContainerKind {
    /// `struct {...}` / `extern struct {...}`.
    Struct { is_extern: bool },
    /// `enum {...}` / `enum(TagType) {...}`.
    Enum { tag: Option<Box<Expr>> },
    /// `union {...}` / `union(enum) {...}` / `union(TagType) {...}`.
    Union { tag: UnionTag },
}

/// The tag clause of a `union`.
#[derive(Clone, Debug, PartialEq)]
pub enum UnionTag {
    /// `union {...}` — bare, no tag.
    None,
    /// `union(enum) {...}` — inferred tag enum.
    Inferred,
    /// `union(TagType) {...}` — explicit tag type.
    Typed(Box<Expr>),
}

/// A member of a container body: a field, or a nested declaration.
#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    /// A struct/union/enum field.
    Field(Field),
    /// A nested declaration (`const`/`var`/`fn`/`test`/`comptime`), reusing
    /// [`Item`].
    Decl(Item),
}

impl Member {
    /// The source span of this member.
    pub fn span(&self) -> Span {
        match self {
            Member::Field(f) => f.span,
            Member::Decl(i) => i.span(),
        }
    }
}

/// A struct/union field, or an enum field (`ty == None` for enum fields, which
/// carry only a name and an optional value).
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// A leading `///` doc comment, if any.
    pub doc: Option<String>,
    /// `true` if the field is `pub`.
    pub is_pub: bool,
    /// `true` for a `comptime` struct field.
    pub is_comptime: bool,
    /// The field name.
    pub name: String,
    /// The field's type (`None` for enum fields).
    pub ty: Option<Expr>,
    /// An optional `align(e)` clause (struct fields only).
    pub align: Option<Expr>,
    /// A default value (`= v`) — struct field default or enum field value.
    pub default: Option<Expr>,
    /// Source span of the whole field.
    pub span: Span,
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
    /// `errdefer [|name|] <stmt-or-expr>;` — cleanup on the error path only.
    Errdefer {
        capture: Option<String>,
        body: Box<Stmt>,
        span: Span,
    },
    /// `return [expr];`
    Return { value: Option<Expr>, span: Span },
    /// A bare expression used for its effect, e.g. `try out.print(...);`.
    Expr { expr: Expr, span: Span },

    /// `target assign_op value;` — assignment (a statement, never an expr).
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
        span: Span,
    },
    /// `comptime { ... }` — a comptime block statement.
    Comptime { body: Vec<Stmt>, span: Span },
    /// A bare `{ ... }` block statement (also a `defer`/`errdefer` body).
    Block { body: Vec<Stmt>, span: Span },

    // ---- Statement forms of control flow (no trailing `;`) ---------------
    /// Statement-form `if`; `expr` is an [`Expr::If`].
    If { expr: Expr, span: Span },
    /// Statement-form `while`; `expr` is an [`Expr::While`].
    While { expr: Expr, span: Span },
    /// Statement-form `for`; `expr` is an [`Expr::For`].
    For { expr: Expr, span: Span },
    /// Statement-form `switch`; `expr` is an [`Expr::Switch`].
    Switch { expr: Expr, span: Span },

    /// `break [:label] [value];`
    Break {
        label: Option<String>,
        value: Option<Expr>,
        span: Span,
    },
    /// `continue [:label];`
    Continue { label: Option<String>, span: Span },
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
            | Stmt::Expr { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Comptime { span, .. }
            | Stmt::Block { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Switch { span, .. }
            | Stmt::Break { span, .. }
            | Stmt::Continue { span, .. } => *span,
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
    /// The parameter name (`_` for a discard, `""` for an unnamed fn-type
    /// parameter such as the `i32` in `fn (i32) bool`).
    pub name: String,
    /// The parameter's type expression. For `anytype` this is an
    /// [`Expr::AnyType`].
    pub ty: Expr,
    /// Source span of the whole parameter.
    pub span: Span,
}

/// A top-level item. At file scope k2 admits only declarations and tests (there
/// are no free statements at the top level — see the grammar's
/// `top_level_decl`).
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A top-level (or nested) `const` declaration, e.g.
    /// `const std = @import("std");`. A container-type declaration is just a
    /// `const` whose `value` is an [`Expr::Container`]. `is_pub` records a
    /// leading `pub`.
    Const {
        doc: Option<String>,
        is_pub: bool,
        name: String,
        ty: Option<Expr>,
        value: Expr,
        span: Span,
    },
    /// A top-level (or nested) `var` declaration.
    Var {
        doc: Option<String>,
        is_pub: bool,
        name: String,
        ty: Option<Expr>,
        value: Option<Expr>,
        span: Span,
    },
    /// A function declaration, e.g. `pub fn main(sys: *System) !void { ... }`.
    /// `ret` is the return-type expression (which may be an error union such as
    /// `!void`); `body` is `None` for an `extern`/proto declaration (`;`).
    Fn {
        doc: Option<String>,
        is_pub: bool,
        is_extern: bool,
        is_export: bool,
        is_inline: bool,
        name: String,
        params: Vec<Param>,
        is_varargs: bool,
        align: Option<Expr>,
        ret: Expr,
        body: Option<Vec<Stmt>>,
        span: Span,
    },
    /// A `test "name" { ... }` declaration. `name` is the raw string-literal or
    /// identifier text, or `None` for a bare `test { ... }`.
    Test {
        doc: Option<String>,
        name: Option<String>,
        body: Vec<Stmt>,
        span: Span,
    },
    /// A top-level `comptime { ... }` block.
    Comptime { body: Vec<Stmt>, span: Span },
}

impl Item {
    /// The source span of this item.
    pub fn span(&self) -> Span {
        match self {
            Item::Const { span, .. }
            | Item::Var { span, .. }
            | Item::Fn { span, .. }
            | Item::Test { span, .. }
            | Item::Comptime { span, .. } => *span,
        }
    }

    /// Constructs a top-level `const` item (with no doc comment).
    pub fn const_item(
        is_pub: bool,
        name: impl Into<String>,
        ty: Option<Expr>,
        value: Expr,
        span: Span,
    ) -> Item {
        Item::Const {
            doc: None,
            is_pub,
            name: name.into(),
            ty,
            value,
            span,
        }
    }
}

/// A whole parsed source file: leading file-level doc comments plus an ordered
/// list of top-level [`Item`]s.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SourceFile {
    /// Leading file-level `///` doc comments, in source order.
    pub doc: Vec<String>,
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
        let sys_ty = Expr::Pointer {
            is_const: false,
            align: None,
            inner: Box::new(Expr::ident("System", Span::new(22, 28, 1, 23))),
            span: Span::new(21, 28, 1, 22),
        };
        let param = Param {
            is_comptime: false,
            name: "sys".into(),
            ty: sys_ty,
            span: Span::new(16, 28, 1, 17),
        };
        // `!void` — an error union with no left (error) operand.
        let ret = Expr::ErrorUnion {
            err: None,
            ok: Box::new(Expr::ident("void", Span::new(31, 35, 1, 32))),
            span: Span::new(30, 35, 1, 31),
        };
        let body = vec![Stmt::Return {
            value: None,
            span: Span::new(38, 45, 1, 39),
        }];
        let item = Item::Fn {
            doc: None,
            is_pub: true,
            is_extern: false,
            is_export: false,
            is_inline: false,
            name: "main".into(),
            params: vec![param],
            is_varargs: false,
            align: None,
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
