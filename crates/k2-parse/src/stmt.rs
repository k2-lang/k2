//! Statement and control-flow parsing.
//!
//! Control-flow constructs (`if`/`while`/`for`/`switch`) are expression-first:
//! one parser builds the [`Expr`] node, and the statement forms simply wrap it
//! in the matching [`Stmt`] variant. The `as_expr` flag tells the shared parser
//! whether `else` is mandatory (expression form) and whether bodies must be
//! blocks (expression form) or may be single nested statements (statement
//! form).

use k2_lexer::TokenKind;
use k2_syntax::{Expr, Span, Stmt, SwitchArm, SwitchItem, SwitchPattern};

use crate::{assign_op_of, Parser};

impl Parser {
    /// Parses a `{ ... }` block, returning its statements and full span. The
    /// cursor must be on the opening `{`.
    pub(crate) fn parse_block(&mut self) -> (Vec<Stmt>, Span) {
        let (_guard, over) = self.enter();
        if over {
            // Too deep: emit the single diagnostic and return an empty block,
            // consuming the matching braces' worth of tokens lazily via the
            // caller's recovery — we just stop descending here.
            let _ = self.too_deep();
            let span = self.here();
            // Skip the unparsed (deeply nested) brace body to a matching close so
            // the outer parser resumes cleanly rather than re-entering it.
            self.skip_balanced_braces();
            return (Vec::new(), span);
        }
        let start = self.expect(TokenKind::LBrace, "to open a block");
        let mut stmts = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            let before = self.pos;
            if let Some(s) = self.parse_statement() {
                stmts.push(s);
            } else {
                self.synchronize_stmt();
            }
            // Loop guard: never spin if a statement consumed nothing.
            if self.pos == before {
                self.bump();
            }
        }
        let end = self.expect(TokenKind::RBrace, "to close a block");
        (stmts, start.merge(end))
    }

    /// Skips a `{ ... }` group with balanced brace nesting *without* recursing,
    /// used only by the recursion-depth bail-out in [`Parser::parse_block`]: the
    /// body is too deeply nested to descend into, so we scan past it iteratively
    /// (bounded by the finite token stream) and leave the cursor just past the
    /// matching `}`. If the cursor is not on a `{`, nothing is consumed.
    fn skip_balanced_braces(&mut self) {
        if !self.at(TokenKind::LBrace) {
            return;
        }
        let mut depth = 0u32;
        while !self.at_eof() {
            match self.cur_kind() {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// Parses one statement, returning `None` to request statement-level
    /// recovery (the caller synchronizes).
    pub(crate) fn parse_statement(&mut self) -> Option<Stmt> {
        let (_guard, over) = self.enter();
        if over {
            // Too deep: emit the single diagnostic and request recovery so the
            // enclosing block loop makes progress without recursing.
            let _ = self.too_deep();
            return None;
        }
        if self.skip_stray_doc() {
            return self.parse_statement();
        }
        let start = self.here();
        match self.cur_kind() {
            TokenKind::KwConst => Some(self.parse_const_stmt()),
            TokenKind::KwVar => Some(self.parse_var_stmt()),
            TokenKind::KwComptime => self.parse_comptime_stmt(start),
            TokenKind::KwDefer => Some(self.parse_defer_stmt(start)),
            TokenKind::KwErrdefer => Some(self.parse_errdefer_stmt(start)),
            TokenKind::KwReturn => Some(self.parse_return_stmt(start)),
            TokenKind::KwBreak => Some(self.parse_break_stmt(start)),
            TokenKind::KwContinue => Some(self.parse_continue_stmt(start)),
            TokenKind::KwIf => {
                let expr = self.parse_if(false);
                let span = expr.span();
                Some(Stmt::If { expr, span })
            }
            TokenKind::KwSwitch => {
                let expr = self.parse_switch();
                let span = expr.span();
                Some(Stmt::Switch { expr, span })
            }
            TokenKind::KwWhile => {
                let expr = self.parse_while(None, false, false);
                let span = expr.span();
                Some(Stmt::While { expr, span })
            }
            TokenKind::KwFor => {
                let expr = self.parse_for(None, false, false);
                let span = expr.span();
                Some(Stmt::For { expr, span })
            }
            TokenKind::KwInline => {
                let expr = self.parse_inline_loop(None, false);
                let span = expr.span();
                Some(self.wrap_loop_stmt(expr, span))
            }
            TokenKind::LBrace => {
                let (body, span) = self.parse_block();
                Some(Stmt::Block { body, span })
            }
            // A labeled loop or labeled block statement: `name : ...`.
            TokenKind::Ident if self.peek_kind(1) == TokenKind::Colon => {
                self.parse_labeled_stmt(start)
            }
            _ => self.parse_expr_or_assign_stmt(start),
        }
    }

    /// Wraps a loop expression in the right statement variant.
    fn wrap_loop_stmt(&self, expr: Expr, span: Span) -> Stmt {
        match expr {
            Expr::For { .. } => Stmt::For { expr, span },
            _ => Stmt::While { expr, span },
        }
    }

    /// Parses a labeled statement: `name : (block | while | for | inline ...)`.
    fn parse_labeled_stmt(&mut self, start: Span) -> Option<Stmt> {
        let label = self.cur().text.clone();
        self.bump(); // name
        self.bump(); // `:`
        match self.cur_kind() {
            TokenKind::LBrace => {
                let (body, bspan) = self.parse_block();
                let span = start.merge(bspan);
                Some(Stmt::Expr {
                    expr: Expr::Block {
                        label: Some(label),
                        body,
                        span,
                    },
                    span,
                })
            }
            TokenKind::KwWhile => {
                let expr = self.parse_while(Some(label), false, false);
                let span = start.merge(expr.span());
                Some(Stmt::While { expr, span })
            }
            TokenKind::KwFor => {
                let expr = self.parse_for(Some(label), false, false);
                let span = start.merge(expr.span());
                Some(Stmt::For { expr, span })
            }
            TokenKind::KwInline => {
                let expr = self.parse_inline_loop(Some(label), false);
                let span = start.merge(expr.span());
                Some(self.wrap_loop_stmt(expr, span))
            }
            _ => {
                let span = self.here();
                self.error(span, "expected a block or loop after a label");
                None
            }
        }
    }

    // ---- declarations (block-level) -------------------------------------

    /// Parses a block-level `const name [: type] = value;`.
    fn parse_const_stmt(&mut self) -> Stmt {
        let start = self.bump(); // `const`
        let name = self.expect_ident_text("after `const`");
        let ty = self.parse_opt_type_annotation();
        self.expect(TokenKind::Eq, "in a `const` declaration");
        let value = self.parse_expr();
        let end = self.expect(TokenKind::Semicolon, "after a `const` declaration");
        Stmt::Const {
            name,
            ty,
            value,
            span: start.merge(end),
        }
    }

    /// Parses a block-level `var name [: type] [= value];`.
    fn parse_var_stmt(&mut self) -> Stmt {
        let start = self.bump(); // `var`
        let name = self.expect_ident_text("after `var`");
        let ty = self.parse_opt_type_annotation();
        let value = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_expr())
        } else {
            None
        };
        let end = self.expect(TokenKind::Semicolon, "after a `var` declaration");
        Stmt::Var {
            name,
            ty,
            value,
            span: start.merge(end),
        }
    }

    /// Parses an optional `: type` annotation.
    pub(crate) fn parse_opt_type_annotation(&mut self) -> Option<Expr> {
        if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        }
    }

    /// Parses `comptime` as a statement: a `comptime { ... }` block, a
    /// `comptime`-qualified decl, or a `comptime EXPR;` expression statement.
    fn parse_comptime_stmt(&mut self, start: Span) -> Option<Stmt> {
        match self.peek_kind(1) {
            TokenKind::LBrace => {
                self.bump(); // `comptime`
                let (body, bspan) = self.parse_block();
                Some(Stmt::Comptime {
                    body,
                    span: start.merge(bspan),
                })
            }
            TokenKind::KwVar => {
                self.bump(); // `comptime`
                Some(self.parse_var_stmt())
            }
            TokenKind::KwConst => {
                self.bump(); // `comptime`
                Some(self.parse_const_stmt())
            }
            _ => self.parse_expr_or_assign_stmt(start),
        }
    }

    /// Parses `defer (block | expr) ;`. A block body needs no trailing `;`
    /// (matching the examples); an expression body is terminated by `;`.
    fn parse_defer_stmt(&mut self, start: Span) -> Stmt {
        self.bump(); // `defer`
        let (body, end) = self.parse_defer_body(start);
        Stmt::Defer {
            body: Box::new(body),
            span: start.merge(end),
        }
    }

    /// Parses `errdefer [|name|] (block | expr) ;`.
    fn parse_errdefer_stmt(&mut self, start: Span) -> Stmt {
        self.bump(); // `errdefer`
        let capture = if self.at(TokenKind::Pipe) {
            self.bump();
            let name = self.expect_ident_text("as the errdefer capture");
            self.expect(TokenKind::Pipe, "to close the errdefer capture");
            Some(name)
        } else {
            None
        };
        let (body, end) = self.parse_defer_body(start);
        Stmt::Errdefer {
            capture,
            body: Box::new(body),
            span: start.merge(end),
        }
    }

    /// Parses a `defer`/`errdefer` body: either a `{ ... }` block (becomes a
    /// [`Stmt::Block`], no trailing `;` required) or a single expression
    /// (becomes a [`Stmt::Expr`], terminated by `;`). Returns the body and the
    /// span of its terminator.
    fn parse_defer_body(&mut self, _start: Span) -> (Stmt, Span) {
        if self.at(TokenKind::LBrace) {
            let (body, span) = self.parse_block();
            // A trailing `;` after the block is allowed but optional.
            let _ = self.eat(TokenKind::Semicolon);
            (Stmt::Block { body, span }, span)
        } else {
            let expr = self.parse_expr();
            let espan = expr.span();
            let end = self.expect(TokenKind::Semicolon, "after a `defer`/`errdefer`");
            (Stmt::Expr { expr, span: espan }, end)
        }
    }

    /// Parses `return [expr];`.
    fn parse_return_stmt(&mut self, start: Span) -> Stmt {
        self.bump(); // `return`
        let value = if self.at(TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_expr())
        };
        let end = self.expect(TokenKind::Semicolon, "after a `return`");
        Stmt::Return {
            value,
            span: start.merge(end),
        }
    }

    /// Parses `break [:label] [expr];`.
    fn parse_break_stmt(&mut self, start: Span) -> Stmt {
        self.bump(); // `break`
        let label = self.parse_opt_label_target();
        let value = if self.at(TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_expr())
        };
        let end = self.expect(TokenKind::Semicolon, "after a `break`");
        Stmt::Break {
            label,
            value,
            span: start.merge(end),
        }
    }

    /// Parses `continue [:label];`.
    fn parse_continue_stmt(&mut self, start: Span) -> Stmt {
        self.bump(); // `continue`
        let label = self.parse_opt_label_target();
        let end = self.expect(TokenKind::Semicolon, "after a `continue`");
        Stmt::Continue {
            label,
            span: start.merge(end),
        }
    }

    /// Parses an optional `: label` target for `break`/`continue`.
    fn parse_opt_label_target(&mut self) -> Option<String> {
        if self.eat(TokenKind::Colon).is_some() {
            Some(self.expect_ident_text("as a `break`/`continue` label"))
        } else {
            None
        }
    }

    /// Parses a bare expression statement or an assignment statement.
    fn parse_expr_or_assign_stmt(&mut self, start: Span) -> Option<Stmt> {
        let target = self.parse_expr();
        if let Some(op) = assign_op_of(self.cur_kind()) {
            self.bump();
            let value = self.parse_expr();
            let end = self.expect(TokenKind::Semicolon, "after an assignment");
            Some(Stmt::Assign {
                target,
                op,
                value,
                span: start.merge(end),
            })
        } else {
            let end = self.expect(TokenKind::Semicolon, "after an expression statement");
            let span = start.merge(end);
            Some(Stmt::Expr { expr: target, span })
        }
    }

    // ---- control flow (shared between expr and stmt forms) ---------------

    /// Parses the parenthesized header expression of a control-flow construct,
    /// with `no_struct_lit` set so a trailing `{` is a block, not a `T{...}`
    /// initializer.
    fn parse_header_expr(&mut self) -> Expr {
        self.expect(TokenKind::LParen, "to open a control-flow header");
        // Inside the parens themselves a struct literal is fine.
        let e = self.parse_expr_no_struct(false);
        self.expect(TokenKind::RParen, "to close a control-flow header");
        e
    }

    /// Parses a control-flow *body*. In expression form the body must be a
    /// block; in statement form it may be a block or a single nested statement.
    fn parse_cf_body(&mut self, as_expr: bool) -> Expr {
        if self.at(TokenKind::LBrace) {
            let (body, span) = self.parse_block();
            Expr::Block {
                label: None,
                body,
                span,
            }
        } else if as_expr {
            // Expression form: the body is an expression.
            self.parse_expr()
        } else {
            // Statement form: a single nested statement, wrapped as an expr-ish
            // block so the node type stays uniform.
            //
            // The grammar's `block_or_stmt = block | statement` allows a single
            // NON-block body before an `else` (spec §4.3:
            // `if (n < 0) -1 else if (n > 0) 1 else 0`). A bare expression /
            // assignment statement normally requires a trailing `;`, but when a
            // following `else` terminates the body we must NOT demand one — the
            // `else` itself terminates the body expression. Other statement kinds
            // (declarations, nested control flow, …) carry their own terminator
            // and are parsed by the general statement parser.
            let span = self.here();
            if let Some(s) = self.parse_cf_body_stmt() {
                let sspan = s.span();
                Expr::Block {
                    label: None,
                    body: vec![s],
                    span: sspan,
                }
            } else {
                self.error_expr(span, "expected a statement as a control-flow body")
            }
        }
    }

    /// Parses the single nested statement of a statement-form control-flow body.
    ///
    /// Identical to [`Parser::parse_statement`] except for the bare
    /// expression/assignment case: there a trailing `else` is accepted as a
    /// terminator in lieu of `;`, so `if (a) x() else y();` and
    /// `while (a) x() else y();` parse without a spurious "expected `;`". When no
    /// `else` follows, a `;` is still required, preserving every existing
    /// single-statement-body diagnostic.
    fn parse_cf_body_stmt(&mut self) -> Option<Stmt> {
        let start = self.here();
        // Only the bare-expression / assignment forms can be terminated by a
        // following `else`; every other statement form already supplies its own
        // terminator, so defer to the normal statement parser for those.
        let defer_to_general = matches!(
            self.cur_kind(),
            TokenKind::KwConst
                | TokenKind::KwVar
                | TokenKind::KwComptime
                | TokenKind::KwDefer
                | TokenKind::KwErrdefer
                | TokenKind::KwReturn
                | TokenKind::KwBreak
                | TokenKind::KwContinue
                | TokenKind::KwIf
                | TokenKind::KwSwitch
                | TokenKind::KwWhile
                | TokenKind::KwFor
                | TokenKind::KwInline
                | TokenKind::LBrace
        ) || (self.at(TokenKind::Ident)
            && self.peek_kind(1) == TokenKind::Colon);
        if defer_to_general {
            return self.parse_statement();
        }
        if self.skip_stray_doc() {
            return self.parse_cf_body_stmt();
        }
        // Bare expression or assignment, with `else` permitted as a terminator.
        let target = self.parse_expr();
        if let Some(op) = assign_op_of(self.cur_kind()) {
            self.bump();
            let value = self.parse_expr();
            let end = self.expect_body_terminator("after an assignment");
            Some(Stmt::Assign {
                target,
                op,
                value,
                span: start.merge(end),
            })
        } else {
            let end = self.expect_body_terminator("after an expression statement");
            let span = start.merge(end);
            Some(Stmt::Expr { expr: target, span })
        }
    }

    /// Consumes the terminator of a statement-form control-flow body: a `;` as
    /// usual, but a following `else` is accepted *without* consuming it (the
    /// enclosing `if`/`while`/`for` parser handles the `else`). Returns the span
    /// of the terminator (the `;`, or a point span at the `else`).
    fn expect_body_terminator(&mut self, ctx: &str) -> Span {
        if self.at(TokenKind::KwElse) {
            let s = self.here();
            Span::point(s.start, s.line, s.col)
        } else {
            self.expect(TokenKind::Semicolon, ctx)
        }
    }

    /// Parses `if (cond) [|cap|] then [else [|cap|] otherwise]`. With
    /// `as_expr`, the `else` branch is required and both branches are
    /// expressions.
    pub(crate) fn parse_if(&mut self, as_expr: bool) -> Expr {
        let start = self.bump(); // `if`
        let cond = self.parse_header_expr();
        let capture = if self.at(TokenKind::Pipe) {
            Some(self.parse_capture())
        } else {
            None
        };
        let then_branch = self.parse_cf_body(as_expr);
        let mut span = start.merge(then_branch.span());

        let (else_capture, else_branch) = if self.at(TokenKind::KwElse) {
            self.bump();
            let cap = if self.at(TokenKind::Pipe) {
                Some(self.parse_capture())
            } else {
                None
            };
            let e = self.parse_cf_body(as_expr);
            span = span.merge(e.span());
            (cap, Some(Box::new(e)))
        } else {
            if as_expr {
                let s = self.here();
                self.error(s, "an `if` expression requires an `else` branch");
            }
            (None, None)
        };

        Expr::If {
            cond: Box::new(cond),
            capture,
            then_branch: Box::new(then_branch),
            else_capture,
            else_branch,
            span,
        }
    }

    /// Parses
    /// `[lbl] [inline] while (cond) [|cap|] [: (cont)] body [else [|cap|] e]`.
    /// `label`/`is_inline` are supplied by the caller (which consumed them).
    pub(crate) fn parse_while(
        &mut self,
        label: Option<String>,
        is_inline: bool,
        as_expr: bool,
    ) -> Expr {
        let start = self.bump(); // `while`
        let cond = self.parse_header_expr();
        let capture = if self.at(TokenKind::Pipe) {
            Some(self.parse_capture())
        } else {
            None
        };
        let cont = if self.at(TokenKind::Colon) {
            self.bump();
            self.expect(TokenKind::LParen, "to open a `while` continue clause");
            let s = self.parse_continue_clause();
            self.expect(TokenKind::RParen, "to close a `while` continue clause");
            Some(Box::new(s))
        } else {
            None
        };
        let body = self.parse_cf_body(as_expr);
        let mut span = start.merge(body.span());

        let (else_capture, else_branch) = self.parse_opt_else(as_expr, &mut span);

        Expr::While {
            label,
            is_inline,
            cond: Box::new(cond),
            capture,
            cont,
            body: Box::new(body),
            else_capture,
            else_branch,
            span,
        }
    }

    /// Parses a `while` continue clause: an inline assignment or an expression,
    /// with no trailing `;`.
    fn parse_continue_clause(&mut self) -> Stmt {
        let start = self.here();
        let target = self.parse_expr();
        if let Some(op) = assign_op_of(self.cur_kind()) {
            self.bump();
            let value = self.parse_expr();
            let span = start.merge(value.span());
            Stmt::Assign {
                target,
                op,
                value,
                span,
            }
        } else {
            let span = target.span();
            Stmt::Expr { expr: target, span }
        }
    }

    /// Parses `[lbl] [inline] for (operands) |captures| body [else e]`.
    pub(crate) fn parse_for(
        &mut self,
        label: Option<String>,
        is_inline: bool,
        as_expr: bool,
    ) -> Expr {
        let start = self.bump(); // `for`
        self.expect(TokenKind::LParen, "to open the `for` operands");
        let saved = self.no_struct_lit;
        self.no_struct_lit = false;
        let operands = self.parse_for_operands();
        self.no_struct_lit = saved;
        self.expect(TokenKind::RParen, "to close the `for` operands");

        let captures = if self.at(TokenKind::Pipe) {
            self.parse_capture().names
        } else {
            let s = self.here();
            self.error(s, "a `for` loop requires a `|capture|`");
            Vec::new()
        };

        let body = self.parse_cf_body(as_expr);
        let mut span = start.merge(body.span());

        let else_branch = if self.at(TokenKind::KwElse) {
            self.bump();
            let e = self.parse_cf_body(as_expr);
            span = span.merge(e.span());
            Some(Box::new(e))
        } else {
            None
        };

        Expr::For {
            label,
            is_inline,
            operands,
            captures,
            body: Box::new(body),
            else_branch,
            span,
        }
    }

    /// Parses an optional `else [|cap|] body` clause shared by `while`. Updates
    /// `span` to include the clause.
    fn parse_opt_else(
        &mut self,
        as_expr: bool,
        span: &mut Span,
    ) -> (Option<k2_syntax::Capture>, Option<Box<Expr>>) {
        if self.at(TokenKind::KwElse) {
            self.bump();
            let cap = if self.at(TokenKind::Pipe) {
                Some(self.parse_capture())
            } else {
                None
            };
            let e = self.parse_cf_body(as_expr);
            *span = span.merge(e.span());
            (cap, Some(Box::new(e)))
        } else {
            (None, None)
        }
    }

    /// Parses `switch (scrutinee) { arms }`.
    pub(crate) fn parse_switch(&mut self) -> Expr {
        let start = self.bump(); // `switch`
        let scrutinee = self.parse_header_expr();
        self.expect(TokenKind::LBrace, "to open a `switch` body");
        let mut arms = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            let before = self.pos;
            arms.push(self.parse_switch_arm());
            if self.pos == before {
                self.bump();
            }
        }
        let end = self.expect(TokenKind::RBrace, "to close a `switch` body");
        Expr::Switch {
            scrutinee: Box::new(scrutinee),
            arms,
            span: start.merge(end),
        }
    }

    /// Parses one `switch` arm: `pattern => [|cap|] body ,`.
    fn parse_switch_arm(&mut self) -> SwitchArm {
        let start = self.here();
        let pattern = if self.at(TokenKind::KwElse) {
            self.bump();
            SwitchPattern::Else
        } else {
            // A pattern is `item {, item}`, terminated by `=>`. Commas separate
            // items; the item list never contains `=>`, so we simply keep
            // consuming items while a comma is followed by another item.
            let mut items = Vec::new();
            loop {
                items.push(self.parse_switch_item());
                if self.at(TokenKind::Comma) && self.peek_kind(1) != TokenKind::FatArrow {
                    self.bump();
                } else {
                    break;
                }
            }
            SwitchPattern::Items(items)
        };
        self.expect(TokenKind::FatArrow, "in a `switch` arm");
        let capture = if self.at(TokenKind::Pipe) {
            Some(self.parse_capture())
        } else {
            None
        };
        let body = if self.at(TokenKind::LBrace) {
            let (b, sp) = self.parse_block();
            Expr::Block {
                label: None,
                body: b,
                span: sp,
            }
        } else {
            self.parse_expr()
        };
        let mut end = body.span();
        if let Some(c) = self.eat(TokenKind::Comma) {
            end = c;
        }
        SwitchArm {
            pattern,
            capture,
            body,
            span: start.merge(end),
        }
    }

    /// Parses one switch item: `expr [... expr]` (inclusive range).
    fn parse_switch_item(&mut self) -> SwitchItem {
        let lo = self.parse_expr();
        if self.at(TokenKind::DotDotDot) {
            self.bump();
            let hi = self.parse_expr();
            let span = lo.span().merge(hi.span());
            SwitchItem {
                lo,
                hi: Some(hi),
                span,
            }
        } else {
            let span = lo.span();
            SwitchItem { lo, hi: None, span }
        }
    }
}
