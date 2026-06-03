//! A deterministic S-expression pretty-printer for the k2 AST.
//!
//! The output is a stable, indented Lisp-like form designed for golden /
//! round-trip testing: parse → [`to_sexpr`] → compare against a checked-in
//! string. Spans are *not* printed by default (they make goldens brittle); the
//! [`to_sexpr_spans`] variant appends `@start..end` to each node for
//! span-debugging.

use k2_syntax::{
    AssignOp, BinOp, Capture, CaptureName, Container, ContainerKind, Expr, Field, FieldInit,
    ForOperand, InitBody, Item, Member, SourceFile, Stmt, SwitchArm, SwitchItem, SwitchPattern,
    UnOp, UnionTag,
};

/// Renders a whole [`SourceFile`] to a canonical S-expression string (no
/// spans).
pub fn to_sexpr(file: &SourceFile) -> String {
    let mut p = Printer {
        out: String::new(),
        spans: false,
        depth: 0,
    };
    p.source_file(file);
    p.out
}

/// Like [`to_sexpr`] but appends `@start..end` to each node for span-debugging.
pub fn to_sexpr_spans(file: &SourceFile) -> String {
    let mut p = Printer {
        out: String::new(),
        spans: true,
        depth: 0,
    };
    p.source_file(file);
    p.out
}

/// The maximum recursion depth the pretty-printer will descend before emitting
/// an `(...)` ellipsis and stopping. The parser already bounds tree depth (see
/// [`crate::MAX_DEPTH`]), but the printer recurses over whatever tree it is
/// handed, so it carries its own independent guard: a maliciously deep (yet
/// now parser-bounded) tree must still print without overflowing the stack.
/// The cap is generous relative to the parser's so well-formed trees are never
/// truncated.
const PRINT_MAX_DEPTH: usize = 1024;

/// Internal printer state: an output buffer, the span-printing flag, and a
/// recursion-depth counter for the expression/statement walk.
struct Printer {
    out: String,
    spans: bool,
    /// Current recursion depth of the [`Printer::expr`]/[`Printer::stmt`] walk,
    /// bounded by [`PRINT_MAX_DEPTH`] to keep printing total even on a
    /// pathologically deep tree.
    depth: usize,
}

impl Printer {
    // ---- low-level emit --------------------------------------------------

    /// Writes `depth * 2` spaces of indentation.
    fn indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str("  ");
        }
    }

    /// Opens a node line: indent, `(name`.
    fn open(&mut self, depth: usize, name: &str) {
        self.indent(depth);
        self.out.push('(');
        self.out.push_str(name);
    }

    /// Appends a single space then `s` on the current line.
    fn word(&mut self, s: &str) {
        self.out.push(' ');
        self.out.push_str(s);
    }

    /// Appends a keyword-flag (e.g. `:pub`) on the current line.
    fn flag(&mut self, name: &str) {
        self.out.push(' ');
        self.out.push_str(name);
    }

    /// Closes a node with `)` and a newline.
    fn close(&mut self) {
        self.out.push_str(")\n");
    }

    /// Closes a node with span annotation (if enabled) then `)` + newline.
    fn close_span(&mut self, span: k2_syntax::Span) {
        if self.spans {
            self.out
                .push_str(&format!(" @{}..{}", span.start, span.end));
        }
        self.close();
    }

    // ---- top level -------------------------------------------------------

    /// Prints a source file.
    fn source_file(&mut self, file: &SourceFile) {
        self.out.push_str("(source-file");
        if self.spans {
            self.out.push_str(" @file");
        }
        self.out.push('\n');
        for d in &file.doc {
            self.indent(1);
            self.out.push_str("(doc ");
            self.out.push_str(&atom(d));
            self.out.push_str(")\n");
        }
        for item in &file.items {
            self.item(1, item);
        }
        self.out.push_str(")\n");
    }

    /// Prints a top-level / nested item.
    fn item(&mut self, depth: usize, item: &Item) {
        match item {
            Item::Const {
                is_pub,
                name,
                ty,
                value,
                ..
            } => {
                self.open(depth, "const");
                if *is_pub {
                    self.flag(":pub");
                }
                self.word(&atom(name));
                self.out.push('\n');
                if let Some(t) = ty {
                    self.labeled(depth + 1, "ty", t);
                }
                self.expr(depth + 1, value);
                self.indent(depth);
                self.close_span(item.span());
            }
            Item::Var {
                is_pub,
                name,
                ty,
                value,
                ..
            } => {
                self.open(depth, "var");
                if *is_pub {
                    self.flag(":pub");
                }
                self.word(&atom(name));
                self.out.push('\n');
                if let Some(t) = ty {
                    self.labeled(depth + 1, "ty", t);
                }
                if let Some(v) = value {
                    self.expr(depth + 1, v);
                }
                self.indent(depth);
                self.close_span(item.span());
            }
            Item::Fn {
                is_pub,
                is_extern,
                is_export,
                is_inline,
                name,
                params,
                is_varargs,
                align,
                ret,
                body,
                ..
            } => {
                self.open(depth, "fn");
                if *is_pub {
                    self.flag(":pub");
                }
                if *is_extern {
                    self.flag(":extern");
                }
                if *is_export {
                    self.flag(":export");
                }
                if *is_inline {
                    self.flag(":inline");
                }
                self.word(&atom(name));
                self.out.push('\n');
                self.params(depth + 1, params, *is_varargs);
                if let Some(a) = align {
                    self.labeled(depth + 1, "align", a);
                }
                self.labeled(depth + 1, "ret", ret);
                match body {
                    Some(stmts) => self.block(depth + 1, stmts),
                    None => {
                        self.indent(depth + 1);
                        self.out.push_str("(proto)\n");
                    }
                }
                self.indent(depth);
                self.close_span(item.span());
            }
            Item::Test { name, body, .. } => {
                self.open(depth, "test");
                if let Some(n) = name {
                    self.word(&atom(n));
                }
                self.out.push('\n');
                self.block(depth + 1, body);
                self.indent(depth);
                self.close_span(item.span());
            }
            Item::Comptime { body, .. } => {
                self.open(depth, "comptime-item");
                self.out.push('\n');
                self.block(depth + 1, body);
                self.indent(depth);
                self.close_span(item.span());
            }
        }
    }

    /// Prints a parameter list node.
    fn params(&mut self, depth: usize, params: &[k2_syntax::Param], varargs: bool) {
        self.indent(depth);
        self.out.push_str("(params");
        if params.is_empty() && !varargs {
            self.out.push_str(")\n");
            return;
        }
        self.out.push('\n');
        for prm in params {
            self.indent(depth + 1);
            self.out.push_str("(param");
            if prm.is_comptime {
                self.flag(":comptime");
            }
            let nm = if prm.name.is_empty() {
                "_anon"
            } else {
                &prm.name
            };
            self.word(&atom(nm));
            self.out.push('\n');
            self.expr(depth + 2, &prm.ty);
            self.indent(depth + 1);
            self.close_span(prm.span);
        }
        if varargs {
            self.indent(depth + 1);
            self.out.push_str("(varargs)\n");
        }
        self.indent(depth);
        self.out.push_str(")\n");
    }

    /// Prints a `(label child)` wrapper around a single expression.
    fn labeled(&mut self, depth: usize, label: &str, e: &Expr) {
        self.indent(depth);
        self.out.push('(');
        self.out.push_str(label);
        self.out.push('\n');
        self.expr(depth + 1, e);
        self.indent(depth);
        self.out.push_str(")\n");
    }

    /// Prints a `(block stmts...)` node.
    fn block(&mut self, depth: usize, stmts: &[Stmt]) {
        self.indent(depth);
        self.out.push_str("(block");
        if stmts.is_empty() {
            self.out.push_str(")\n");
            return;
        }
        self.out.push('\n');
        for s in stmts {
            self.stmt(depth + 1, s);
        }
        self.indent(depth);
        self.out.push_str(")\n");
    }

    // ---- statements ------------------------------------------------------

    /// Prints a statement.
    fn stmt(&mut self, depth: usize, stmt: &Stmt) {
        // Depth-guard the recursive walk: bail with an ellipsis past the cap.
        if self.descend(depth, stmt.span()) {
            return;
        }
        self.print_stmt(depth, stmt);
        self.depth -= 1;
    }

    /// The body of [`Printer::stmt`], split out so the depth guard wraps it.
    fn print_stmt(&mut self, depth: usize, stmt: &Stmt) {
        match stmt {
            Stmt::Const {
                name, ty, value, ..
            } => {
                self.open(depth, "const");
                self.word(&atom(name));
                self.out.push('\n');
                if let Some(t) = ty {
                    self.labeled(depth + 1, "ty", t);
                }
                self.expr(depth + 1, value);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Var {
                name, ty, value, ..
            } => {
                self.open(depth, "var");
                self.word(&atom(name));
                self.out.push('\n');
                if let Some(t) = ty {
                    self.labeled(depth + 1, "ty", t);
                }
                if let Some(v) = value {
                    self.expr(depth + 1, v);
                }
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Defer { body, .. } => {
                self.open(depth, "defer");
                self.out.push('\n');
                self.stmt(depth + 1, body);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Errdefer { capture, body, .. } => {
                self.open(depth, "errdefer");
                if let Some(c) = capture {
                    self.word(&atom(c));
                }
                self.out.push('\n');
                self.stmt(depth + 1, body);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Return { value, .. } => {
                self.open(depth, "return");
                if let Some(v) = value {
                    self.out.push('\n');
                    self.expr(depth + 1, v);
                    self.indent(depth);
                }
                self.close_span(stmt.span());
            }
            Stmt::Expr { expr, .. } => {
                self.open(depth, "expr");
                self.out.push('\n');
                self.expr(depth + 1, expr);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Assign {
                target, op, value, ..
            } => {
                self.open(depth, "assign");
                self.word(assign_op_name(*op));
                self.out.push('\n');
                self.expr(depth + 1, target);
                self.expr(depth + 1, value);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Comptime { body, .. } => {
                self.open(depth, "comptime");
                self.out.push('\n');
                self.block(depth + 1, body);
                self.indent(depth);
                self.close_span(stmt.span());
            }
            Stmt::Block { body, .. } => {
                self.block(depth, body);
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => {
                self.expr(depth, expr);
            }
            Stmt::Break { label, value, .. } => {
                self.open(depth, "break");
                if let Some(l) = label {
                    self.flag(&format!(":label {}", atom(l)));
                }
                if let Some(v) = value {
                    self.out.push('\n');
                    self.expr(depth + 1, v);
                    self.indent(depth);
                }
                self.close_span(stmt.span());
            }
            Stmt::Continue { label, .. } => {
                self.open(depth, "continue");
                if let Some(l) = label {
                    self.flag(&format!(":label {}", atom(l)));
                }
                self.close_span(stmt.span());
            }
        }
    }

    // ---- expressions -----------------------------------------------------

    /// Prints an expression node.
    fn expr(&mut self, depth: usize, e: &Expr) {
        // Depth-guard the recursive walk: bail with an ellipsis past the cap.
        if self.descend(depth, e.span()) {
            return;
        }
        self.print_expr(depth, e);
        self.depth -= 1;
    }

    /// Enters one level of the print walk: increments [`Printer::depth`] and, if
    /// past [`PRINT_MAX_DEPTH`], emits an `(...)` ellipsis leaf, decrements back,
    /// and returns `true` to tell the caller to stop. Returns `false` (with the
    /// depth left incremented) when it is safe to keep printing.
    fn descend(&mut self, depth: usize, span: k2_syntax::Span) -> bool {
        self.depth += 1;
        if self.depth > PRINT_MAX_DEPTH {
            self.leaf0(depth, "...", span);
            self.depth -= 1;
            true
        } else {
            false
        }
    }

    /// The body of [`Printer::expr`], split out so the depth guard wraps it.
    fn print_expr(&mut self, depth: usize, e: &Expr) {
        match e {
            Expr::Int { text, .. } => self.leaf(depth, "int", &atom(text), e.span()),
            Expr::Float { text, .. } => self.leaf(depth, "float", &atom(text), e.span()),
            Expr::Str { text, .. } => self.leaf(depth, "str", &atom(text), e.span()),
            Expr::Char { text, .. } => self.leaf(depth, "char", &atom(text), e.span()),
            Expr::Bool { value, .. } => self.leaf(
                depth,
                "bool",
                if *value { "true" } else { "false" },
                e.span(),
            ),
            Expr::Null { .. } => self.leaf0(depth, "null", e.span()),
            Expr::Undefined { .. } => self.leaf0(depth, "undefined", e.span()),
            Expr::Unreachable { .. } => self.leaf0(depth, "unreachable", e.span()),
            Expr::AnyType { .. } => self.leaf0(depth, "anytype", e.span()),
            Expr::Ident { name, .. } => self.leaf(depth, "ident", &atom(name), e.span()),
            Expr::EnumLiteral { name, .. } => self.leaf(depth, "enum-lit", &atom(name), e.span()),
            Expr::ErrorLiteral { name, .. } => self.leaf(depth, "error-lit", &atom(name), e.span()),
            Expr::Builtin { name, args, .. } => {
                self.open(depth, "builtin");
                self.word(&atom(name));
                self.children(depth, args.iter(), Self::expr);
                self.close_finish(depth, args.is_empty(), e.span());
            }
            Expr::Field { base, field, .. } => {
                self.open(depth, "field");
                self.word(&atom(field));
                self.out.push('\n');
                self.expr(depth + 1, base);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Call { callee, args, .. } => {
                self.open(depth, "call");
                self.out.push('\n');
                self.expr(depth + 1, callee);
                for a in args {
                    self.expr(depth + 1, a);
                }
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                self.open(depth, "binary");
                self.word(bin_op_name(*op));
                self.out.push('\n');
                self.expr(depth + 1, lhs);
                self.expr(depth + 1, rhs);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Unary { op, operand, .. } => {
                self.open(depth, "unary");
                self.word(un_op_name(*op));
                self.out.push('\n');
                self.expr(depth + 1, operand);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Optional { inner, .. } => self.wrap1(depth, "optional", inner, e.span()),
            Expr::Pointer {
                is_const,
                align,
                inner,
                ..
            } => {
                self.open(depth, "ptr");
                if *is_const {
                    self.flag(":const");
                }
                self.out.push('\n');
                if let Some(a) = align {
                    self.labeled(depth + 1, "align", a);
                }
                self.expr(depth + 1, inner);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Slice {
                is_const,
                align,
                inner,
                ..
            } => {
                self.open(depth, "slice");
                if *is_const {
                    self.flag(":const");
                }
                self.out.push('\n');
                if let Some(a) = align {
                    self.labeled(depth + 1, "align", a);
                }
                self.expr(depth + 1, inner);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::ArrayType { len, inner, .. } => {
                self.open(depth, "array-type");
                self.out.push('\n');
                self.labeled(depth + 1, "len", len);
                self.expr(depth + 1, inner);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::ErrorUnion { err, ok, .. } => {
                self.open(depth, "err-union");
                self.out.push('\n');
                match err {
                    Some(er) => self.labeled(depth + 1, "err", er),
                    None => {
                        self.indent(depth + 1);
                        self.out.push_str("(err)\n");
                    }
                }
                self.labeled(depth + 1, "ok", ok);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::FnType {
                params,
                is_varargs,
                ret,
                ..
            } => {
                self.open(depth, "fn-type");
                self.out.push('\n');
                self.params(depth + 1, params, *is_varargs);
                self.labeled(depth + 1, "ret", ret);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::ErrorSet { fields, .. } => {
                self.open(depth, "error-set");
                for f in fields {
                    self.word(&atom(f));
                }
                self.close_span(e.span());
            }
            Expr::Container(c) => self.container(depth, c),
            Expr::Init { ty, body, .. } => {
                self.open(depth, "init");
                self.out.push('\n');
                match ty {
                    Some(t) => self.labeled(depth + 1, "ty", t),
                    None => {
                        self.indent(depth + 1);
                        self.out.push_str("(ty)\n");
                    }
                }
                self.init_body(depth + 1, body);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Index { base, index, .. } => {
                self.open(depth, "index");
                self.out.push('\n');
                self.expr(depth + 1, base);
                self.expr(depth + 1, index);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::SliceExpr { base, lo, hi, .. } => {
                self.open(depth, "slice-expr");
                self.out.push('\n');
                self.expr(depth + 1, base);
                self.labeled(depth + 1, "lo", lo);
                match hi {
                    Some(h) => self.labeled(depth + 1, "hi", h),
                    None => {
                        self.indent(depth + 1);
                        self.out.push_str("(hi)\n");
                    }
                }
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Deref { base, .. } => self.wrap1(depth, "deref", base, e.span()),
            Expr::Unwrap { base, .. } => self.wrap1(depth, "unwrap", base, e.span()),
            Expr::Catch {
                lhs, capture, rhs, ..
            } => {
                self.open(depth, "catch");
                if let Some(c) = capture {
                    self.word(&atom(c));
                }
                self.out.push('\n');
                self.expr(depth + 1, lhs);
                self.expr(depth + 1, rhs);
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Comptime { inner, .. } => self.wrap1(depth, "comptime", inner, e.span()),
            Expr::Block { label, body, .. } => {
                // A block *expression* prints as a single `(block ...)` node with
                // its statements directly inside (no extra wrapping block).
                self.open(depth, "block");
                if let Some(l) = label {
                    self.flag(&format!(":label {}", atom(l)));
                }
                if body.is_empty() {
                    self.close_span(e.span());
                } else {
                    self.out.push('\n');
                    for s in body {
                        self.stmt(depth + 1, s);
                    }
                    self.indent(depth);
                    self.close_span(e.span());
                }
            }
            Expr::If {
                cond,
                capture,
                then_branch,
                else_capture,
                else_branch,
                ..
            } => {
                self.open(depth, "if");
                self.out.push('\n');
                self.labeled(depth + 1, "cond", cond);
                if let Some(c) = capture {
                    self.capture(depth + 1, c);
                }
                self.labeled(depth + 1, "then", then_branch);
                if let Some(c) = else_capture {
                    self.capture(depth + 1, c);
                }
                if let Some(eb) = else_branch {
                    self.labeled(depth + 1, "else", eb);
                }
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::While {
                label,
                is_inline,
                cond,
                capture,
                cont,
                body,
                else_capture,
                else_branch,
                ..
            } => {
                self.open(depth, "while");
                if *is_inline {
                    self.flag(":inline");
                }
                if let Some(l) = label {
                    self.flag(&format!(":label {}", atom(l)));
                }
                self.out.push('\n');
                self.labeled(depth + 1, "cond", cond);
                if let Some(c) = capture {
                    self.capture(depth + 1, c);
                }
                if let Some(c) = cont {
                    self.indent(depth + 1);
                    self.out.push_str("(cont\n");
                    self.stmt(depth + 2, c);
                    self.indent(depth + 1);
                    self.out.push_str(")\n");
                }
                self.labeled(depth + 1, "body", body);
                if let Some(c) = else_capture {
                    self.capture(depth + 1, c);
                }
                if let Some(eb) = else_branch {
                    self.labeled(depth + 1, "else", eb);
                }
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::For {
                label,
                is_inline,
                operands,
                captures,
                body,
                else_branch,
                ..
            } => {
                self.open(depth, "for");
                if *is_inline {
                    self.flag(":inline");
                }
                if let Some(l) = label {
                    self.flag(&format!(":label {}", atom(l)));
                }
                self.out.push('\n');
                self.for_operands(depth + 1, operands);
                self.capture_names(depth + 1, captures);
                self.labeled(depth + 1, "body", body);
                if let Some(eb) = else_branch {
                    self.labeled(depth + 1, "else", eb);
                }
                self.indent(depth);
                self.close_span(e.span());
            }
            Expr::Switch {
                scrutinee, arms, ..
            } => {
                self.open(depth, "switch");
                self.out.push('\n');
                self.labeled(depth + 1, "on", scrutinee);
                for arm in arms {
                    self.switch_arm(depth + 1, arm);
                }
                self.indent(depth);
                self.close_span(e.span());
            }
        }
    }

    // ---- expression helpers ---------------------------------------------

    /// Emits a leaf node `(name word)` with no children.
    fn leaf(&mut self, depth: usize, name: &str, word: &str, span: k2_syntax::Span) {
        self.open(depth, name);
        self.word(word);
        self.close_span(span);
    }

    /// Emits a zero-argument leaf node `(name)`.
    fn leaf0(&mut self, depth: usize, name: &str, span: k2_syntax::Span) {
        self.open(depth, name);
        self.close_span(span);
    }

    /// Emits a one-child wrapper `(name child)`.
    fn wrap1(&mut self, depth: usize, name: &str, child: &Expr, span: k2_syntax::Span) {
        self.open(depth, name);
        self.out.push('\n');
        self.expr(depth + 1, child);
        self.indent(depth);
        self.close_span(span);
    }

    /// Emits child expressions (used after a same-line header word). Pushes a
    /// newline first if there is at least one child.
    fn children<'a, I>(&mut self, depth: usize, iter: I, f: fn(&mut Self, usize, &Expr))
    where
        I: Iterator<Item = &'a Expr>,
    {
        let mut any = false;
        let items: Vec<&Expr> = iter.collect();
        if !items.is_empty() {
            self.out.push('\n');
            any = true;
        }
        for it in items {
            f(self, depth + 1, it);
        }
        if any {
            self.indent(depth);
        }
    }

    /// Closes a node opened by [`Self::children`]: the indent is already placed
    /// when there were children, otherwise emit nothing extra.
    fn close_finish(&mut self, _depth: usize, _empty: bool, span: k2_syntax::Span) {
        self.close_span(span);
    }

    /// Prints an init-literal body.
    fn init_body(&mut self, depth: usize, body: &InitBody) {
        match body {
            InitBody::Fields(fields) => {
                self.indent(depth);
                self.out.push_str("(fields");
                if fields.is_empty() {
                    self.out.push_str(")\n");
                    return;
                }
                self.out.push('\n');
                for f in fields {
                    self.field_init(depth + 1, f);
                }
                self.indent(depth);
                self.out.push_str(")\n");
            }
            InitBody::Tuple(elems) => {
                self.indent(depth);
                self.out.push_str("(tuple");
                if elems.is_empty() {
                    self.out.push_str(")\n");
                    return;
                }
                self.out.push('\n');
                for el in elems {
                    self.expr(depth + 1, el);
                }
                self.indent(depth);
                self.out.push_str(")\n");
            }
        }
    }

    /// Prints one `.name = value` field init.
    fn field_init(&mut self, depth: usize, f: &FieldInit) {
        self.open(depth, "field-init");
        self.word(&atom(&f.name));
        self.out.push('\n');
        self.expr(depth + 1, &f.value);
        self.indent(depth);
        self.out.push_str(")\n");
    }

    /// Prints a capture clause node.
    fn capture(&mut self, depth: usize, c: &Capture) {
        self.indent(depth);
        self.out.push_str("(capture");
        for n in &c.names {
            self.capture_name_inline(n);
        }
        self.out.push_str(")\n");
    }

    /// Prints a bare list of `for` capture names as a capture node.
    fn capture_names(&mut self, depth: usize, names: &[CaptureName]) {
        self.indent(depth);
        self.out.push_str("(capture");
        for n in names {
            self.capture_name_inline(n);
        }
        self.out.push_str(")\n");
    }

    /// Appends one capture name on the current line: `x` or `(ref x)`.
    fn capture_name_inline(&mut self, n: &CaptureName) {
        self.out.push(' ');
        if n.by_ref {
            self.out.push_str("(ref ");
            self.out.push_str(&atom(&n.name));
            self.out.push(')');
        } else {
            self.out.push_str(&atom(&n.name));
        }
    }

    /// Prints `for` operands.
    fn for_operands(&mut self, depth: usize, operands: &[ForOperand]) {
        self.indent(depth);
        self.out.push_str("(operands\n");
        for op in operands {
            match op {
                ForOperand::Value(e) => self.expr(depth + 1, e),
                ForOperand::Range { lo, hi, .. } => {
                    self.indent(depth + 1);
                    self.out.push_str("(range\n");
                    self.labeled(depth + 2, "lo", lo);
                    match hi {
                        Some(h) => self.labeled(depth + 2, "hi", h),
                        None => {
                            self.indent(depth + 2);
                            self.out.push_str("(hi)\n");
                        }
                    }
                    self.indent(depth + 1);
                    self.out.push_str(")\n");
                }
            }
        }
        self.indent(depth);
        self.out.push_str(")\n");
    }

    /// Prints one switch arm.
    fn switch_arm(&mut self, depth: usize, arm: &SwitchArm) {
        self.indent(depth);
        self.out.push_str("(arm\n");
        self.switch_pattern(depth + 1, &arm.pattern);
        if let Some(c) = &arm.capture {
            self.capture(depth + 1, c);
        }
        self.labeled(depth + 1, "body", &arm.body);
        self.indent(depth);
        self.out.push_str(")\n");
    }

    /// Prints a switch arm pattern.
    fn switch_pattern(&mut self, depth: usize, pat: &SwitchPattern) {
        match pat {
            SwitchPattern::Else => {
                self.indent(depth);
                self.out.push_str("(else)\n");
            }
            SwitchPattern::Items(items) => {
                self.indent(depth);
                self.out.push_str("(pattern\n");
                for it in items {
                    self.switch_item(depth + 1, it);
                }
                self.indent(depth);
                self.out.push_str(")\n");
            }
        }
    }

    /// Prints one switch item (a value or an inclusive range).
    fn switch_item(&mut self, depth: usize, item: &SwitchItem) {
        match &item.hi {
            None => self.expr(depth, &item.lo),
            Some(hi) => {
                self.indent(depth);
                self.out.push_str("(range\n");
                self.labeled(depth + 1, "lo", &item.lo);
                self.labeled(depth + 1, "hi", hi);
                self.indent(depth);
                self.out.push_str(")\n");
            }
        }
    }

    // ---- containers ------------------------------------------------------

    /// Prints a container type node.
    fn container(&mut self, depth: usize, c: &Container) {
        self.open(depth, "container");
        match &c.kind {
            ContainerKind::Struct { is_extern } => {
                self.word("struct");
                if *is_extern {
                    self.flag(":extern");
                }
                self.out.push('\n');
            }
            ContainerKind::Enum { tag } => {
                self.word("enum");
                self.out.push('\n');
                if let Some(t) = tag {
                    self.labeled(depth + 1, "tag", t);
                }
            }
            ContainerKind::Union { tag } => {
                self.word("union");
                self.out.push('\n');
                match tag {
                    UnionTag::None => {}
                    UnionTag::Inferred => {
                        self.indent(depth + 1);
                        self.out.push_str("(tag-enum)\n");
                    }
                    UnionTag::Typed(t) => self.labeled(depth + 1, "tag", t),
                }
            }
        }
        for m in &c.members {
            self.member(depth + 1, m);
        }
        self.indent(depth);
        self.close_span(c.span);
    }

    /// Prints one container member.
    fn member(&mut self, depth: usize, m: &Member) {
        match m {
            Member::Field(f) => self.field(depth, f),
            Member::Decl(item) => {
                self.indent(depth);
                self.out.push_str("(decl\n");
                self.item(depth + 1, item);
                self.indent(depth);
                self.out.push_str(")\n");
            }
        }
    }

    /// Prints a struct/enum/union field.
    fn field(&mut self, depth: usize, f: &Field) {
        self.open(depth, "field");
        if f.is_pub {
            self.flag(":pub");
        }
        if f.is_comptime {
            self.flag(":comptime");
        }
        self.word(&atom(&f.name));
        self.out.push('\n');
        if let Some(t) = &f.ty {
            self.labeled(depth + 1, "ty", t);
        }
        if let Some(a) = &f.align {
            self.labeled(depth + 1, "align", a);
        }
        if let Some(d) = &f.default {
            self.labeled(depth + 1, "default", d);
        }
        self.indent(depth);
        self.close_span(f.span);
    }
}

/// The S-expression name for a binary operator.
fn bin_op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        BinOp::Concat => "concat",
        BinOp::BitAnd => "bitand",
        BinOp::BitOr => "bitor",
        BinOp::BitXor => "bitxor",
        BinOp::Shl => "shl",
        BinOp::Shr => "shr",
        BinOp::Eq => "eq",
        BinOp::Ne => "ne",
        BinOp::Lt => "lt",
        BinOp::Le => "le",
        BinOp::Gt => "gt",
        BinOp::Ge => "ge",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Orelse => "orelse",
        BinOp::ErrSetMerge => "errset-merge",
    }
}

/// The S-expression name for a unary operator.
fn un_op_name(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "neg",
        UnOp::BitNot => "bitnot",
        UnOp::Not => "not",
        UnOp::AddrOf => "addrof",
        UnOp::Try => "try",
    }
}

/// The S-expression token for an assignment operator.
fn assign_op_name(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Eq => "=",
        AssignOp::AddEq => "+=",
        AssignOp::SubEq => "-=",
        AssignOp::MulEq => "*=",
        AssignOp::DivEq => "/=",
        AssignOp::RemEq => "%=",
        AssignOp::AndEq => "&=",
        AssignOp::OrEq => "|=",
        AssignOp::XorEq => "^=",
        AssignOp::ShlEq => "<<=",
        AssignOp::ShrEq => ">>=",
    }
}

/// Renders an atom: bare when it is a "simple" token (letters/digits/`_`/`@`/`.`
/// and nonempty), otherwise quoted with `\`, `"`, and whitespace escaped, so a
/// multiline string literal stays on one output line.
fn atom(s: &str) -> String {
    let simple = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '@' || c == '.' || c == '-' || c == '/'
        });
    if simple {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    }
}
