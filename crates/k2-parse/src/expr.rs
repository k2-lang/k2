//! Expression parsing: the precedence cascade, postfix chains, and primaries.
//!
//! The cascade runs from lowest precedence (`orelse`/`catch`) to highest
//! (postfix/primary), one function per level, matching the EBNF in
//! `docs/grammar.ebnf` section 6. Because k2 types are expressions, the unary
//! level also handles the type-constructor prefixes (`?`, `*`, `[]`, `[N]`,
//! `!T`); see [`Parser::parse_type`](crate::Parser::parse_type) for the infix
//! `E!T` form that only applies in type position.

use k2_lexer::TokenKind;
use k2_syntax::{BinOp, Capture, CaptureName, Expr, FieldInit, ForOperand, InitBody, Span, UnOp};

use crate::Parser;

impl Parser {
    /// Parses a full expression (the entry point for value positions).
    pub(crate) fn parse_expr(&mut self) -> Expr {
        self.parse_coalesce()
    }

    /// Level 12: `orelse` / `catch` (left-associative). `catch` may bind an
    /// `|err|` capture, so it becomes an [`Expr::Catch`]; `orelse` is a plain
    /// [`BinOp::Orelse`].
    ///
    /// This is the entry to the recursive expression cascade, so it carries the
    /// recursion-depth guard: past [`MAX_DEPTH`](crate::MAX_DEPTH) it bails with
    /// a single "nesting too deep" diagnostic instead of recursing further.
    fn parse_coalesce(&mut self) -> Expr {
        let (_guard, over) = self.enter();
        if over {
            return self.too_deep();
        }
        let mut lhs = self.parse_or();
        loop {
            if self.depth_exceeded {
                break;
            }
            match self.cur_kind() {
                TokenKind::KwOrelse => {
                    self.bump();
                    let rhs = self.parse_or();
                    lhs = Expr::binary(BinOp::Orelse, lhs, rhs);
                }
                TokenKind::KwCatch => {
                    self.bump();
                    let capture = self.parse_catch_capture();
                    let rhs = self.parse_or();
                    let span = lhs.span().merge(rhs.span());
                    lhs = Expr::Catch {
                        lhs: Box::new(lhs),
                        capture,
                        rhs: Box::new(rhs),
                        span,
                    };
                }
                _ => break,
            }
        }
        lhs
    }

    /// Parses an optional `| name |` capture after `catch`.
    fn parse_catch_capture(&mut self) -> Option<String> {
        if self.at(TokenKind::Pipe) {
            self.bump();
            let name = self.expect_capture_name();
            self.expect(TokenKind::Pipe, "to close the catch capture");
            Some(name)
        } else {
            None
        }
    }

    /// Level 11: logical `or`.
    ///
    /// Every binary level's loop also stops on [`Parser::depth_exceeded`]: once
    /// the recursion cap has been hit, the operand parsers return trivial error
    /// nodes, and continuing to consume the remaining operators would rebuild an
    /// unbounded left-leaning spine (which then overflows on the tree's recursive
    /// `Drop`/print). Stopping winds the whole expression down to recovery.
    fn parse_or(&mut self) -> Expr {
        let mut lhs = self.parse_and();
        while self.at(TokenKind::KwOr) && !self.depth_exceeded {
            self.bump();
            let rhs = self.parse_and();
            lhs = Expr::binary(BinOp::Or, lhs, rhs);
        }
        lhs
    }

    /// Level 10: logical `and`.
    fn parse_and(&mut self) -> Expr {
        let mut lhs = self.parse_compare();
        while self.at(TokenKind::KwAnd) && !self.depth_exceeded {
            self.bump();
            let rhs = self.parse_compare();
            lhs = Expr::binary(BinOp::And, lhs, rhs);
        }
        lhs
    }

    /// Level 9: comparison — NON-associative (at most one comparator). A second
    /// comparator is a diagnostic, not a chain.
    fn parse_compare(&mut self) -> Expr {
        let lhs = self.parse_bitor();
        if let Some(op) = compare_op(self.cur_kind()) {
            self.bump();
            let rhs = self.parse_bitor();
            let mut result = Expr::binary(op, lhs, rhs);
            // A second comparator (`a < b < c`) is non-associative: emit ONE
            // diagnostic, then *consume and absorb* each extra comparator and its
            // right operand so we don't hand the statement parser a dangling
            // `< c` (which would cascade into further spurious errors). The loop
            // soaks up a longer chain (`a < b < c < d`) under a single error.
            if compare_op(self.cur_kind()).is_some() {
                let span = self.here();
                self.error(
                    span,
                    "comparison operators are non-associative; parenthesize",
                );
                while let Some(extra) = compare_op(self.cur_kind()) {
                    if self.depth_exceeded {
                        break;
                    }
                    self.bump();
                    let extra_rhs = self.parse_bitor();
                    result = Expr::binary(extra, result, extra_rhs);
                }
            }
            result
        } else {
            lhs
        }
    }

    /// Level 8: bitwise or `|`, and the `||` error-set-merge operator (two
    /// adjacent `Pipe` tokens with no gap — see the reconciliation notes).
    fn parse_bitor(&mut self) -> Expr {
        let mut lhs = self.parse_bitxor();
        while self.at(TokenKind::Pipe) && !self.depth_exceeded {
            // `||` = two *adjacent* `Pipe` tokens → error-set union.
            if self.peek_kind(1) == TokenKind::Pipe
                && self.span_of(self.pos).end == self.span_of(self.pos + 1).start
            {
                self.bump();
                self.bump();
                let rhs = self.parse_bitxor();
                lhs = Expr::binary(BinOp::ErrSetMerge, lhs, rhs);
            } else {
                self.bump();
                let rhs = self.parse_bitxor();
                lhs = Expr::binary(BinOp::BitOr, lhs, rhs);
            }
        }
        lhs
    }

    /// Level 7: bitwise xor `^`.
    fn parse_bitxor(&mut self) -> Expr {
        let mut lhs = self.parse_bitand();
        while self.at(TokenKind::Caret) && !self.depth_exceeded {
            self.bump();
            let rhs = self.parse_bitand();
            lhs = Expr::binary(BinOp::BitXor, lhs, rhs);
        }
        lhs
    }

    /// Level 6: bitwise and `&`.
    fn parse_bitand(&mut self) -> Expr {
        let mut lhs = self.parse_shift();
        while self.at(TokenKind::Amp) && !self.depth_exceeded {
            self.bump();
            let rhs = self.parse_shift();
            lhs = Expr::binary(BinOp::BitAnd, lhs, rhs);
        }
        lhs
    }

    /// Level 5: shifts `<<` `>>`.
    fn parse_shift(&mut self) -> Expr {
        let mut lhs = self.parse_add();
        loop {
            if self.depth_exceeded {
                break;
            }
            let op = match self.cur_kind() {
                TokenKind::Shl => BinOp::Shl,
                TokenKind::Shr => BinOp::Shr,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_add();
            lhs = Expr::binary(op, lhs, rhs);
        }
        lhs
    }

    /// Level 4: additive `+` `-` and concatenation `++`.
    fn parse_add(&mut self) -> Expr {
        let mut lhs = self.parse_mul();
        loop {
            if self.depth_exceeded {
                break;
            }
            let op = match self.cur_kind() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                TokenKind::PlusPlus => BinOp::Concat,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul();
            lhs = Expr::binary(op, lhs, rhs);
        }
        lhs
    }

    /// Level 3: multiplicative `*` `/` `%`.
    fn parse_mul(&mut self) -> Expr {
        let mut lhs = self.parse_unary();
        loop {
            if self.depth_exceeded {
                break;
            }
            let op = match self.cur_kind() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Rem,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary();
            lhs = Expr::binary(op, lhs, rhs);
        }
        lhs
    }

    /// Level 2: prefix/unary operators (right-associative) AND the
    /// type-constructor prefixes (`?`, `*`, `[]`, `[N]`, `!T`), since types are
    /// expressions and stack right-to-left.
    pub(crate) fn parse_unary(&mut self) -> Expr {
        let (_guard, over) = self.enter();
        if over {
            return self.too_deep();
        }
        let start = self.here();
        match self.cur_kind() {
            TokenKind::Minus => self.unary_op(UnOp::Neg, start),
            TokenKind::Tilde => self.unary_op(UnOp::BitNot, start),
            TokenKind::KwNot => self.unary_op(UnOp::Not, start),
            TokenKind::Amp => self.unary_op(UnOp::AddrOf, start),
            TokenKind::KwTry => self.unary_op(UnOp::Try, start),
            TokenKind::KwComptime => {
                self.bump();
                let inner = self.parse_unary();
                let span = start.merge(inner.span());
                Expr::Comptime {
                    inner: Box::new(inner),
                    span,
                }
            }
            TokenKind::Question => {
                self.bump();
                let inner = self.parse_unary();
                let span = start.merge(inner.span());
                Expr::Optional {
                    inner: Box::new(inner),
                    span,
                }
            }
            TokenKind::Star => {
                // Pointer type: `*`, then `const`/`align(e)` in any order, T.
                self.bump();
                let (is_const, align) = self.parse_ptr_qualifiers();
                let inner = self.parse_unary();
                let span = start.merge(inner.span());
                Expr::Pointer {
                    is_const,
                    align: align.map(Box::new),
                    inner: Box::new(inner),
                    span,
                }
            }
            TokenKind::Bang => {
                // `!T` — error union with no left operand.
                self.bump();
                let ok = self.parse_unary();
                let span = start.merge(ok.span());
                Expr::ErrorUnion {
                    err: None,
                    ok: Box::new(ok),
                    span,
                }
            }
            TokenKind::LBracket => self.parse_bracket_type(start),
            _ => self.parse_postfix(),
        }
    }

    /// Helper: parse `op` then a right-associative unary operand.
    fn unary_op(&mut self, op: UnOp, start: Span) -> Expr {
        self.bump();
        let operand = self.parse_unary();
        let span = start.merge(operand.span());
        Expr::Unary {
            op,
            operand: Box::new(operand),
            span,
        }
    }

    /// Parses a `[`-introduced type: a slice type `[]T` / `[]const T` /
    /// `[]align(e) T`, a many-item pointer `[*]T` (modelled as a [`Expr::Slice`]
    /// since they share the postfix-modifier shape), or an array type `[N]T`
    /// (`N` may be `_`).
    fn parse_bracket_type(&mut self, start: Span) -> Expr {
        self.bump(); // `[`
                     // `[*]T` — a many-item pointer. We accept `*` (optionally `[*:s]`-style
                     // sentinels are out of scope) and model it like a slice modifier.
        if self.at(TokenKind::Star) {
            self.bump();
            self.expect(TokenKind::RBracket, "to close a many-item pointer `[*]`");
            let (is_const, align) = self.parse_ptr_qualifiers();
            let inner = self.parse_unary();
            let span = start.merge(inner.span());
            return Expr::Slice {
                is_const,
                align: align.map(Box::new),
                inner: Box::new(inner),
                span,
            };
        }
        if self.at(TokenKind::RBracket) {
            // Slice type `[]...`.
            self.bump();
            let (is_const, align) = self.parse_ptr_qualifiers();
            let inner = self.parse_unary();
            let span = start.merge(inner.span());
            Expr::Slice {
                is_const,
                align: align.map(Box::new),
                inner: Box::new(inner),
                span,
            }
        } else {
            // Array type `[N]T`. The length is a full expression (accepts `_`).
            // Inside the brackets a struct literal is fine, so clear the flag.
            let len = self.parse_expr_no_struct(false);
            self.expect(TokenKind::RBracket, "to close the array length");
            // Parse the ELEMENT type with `no_struct_lit` set so a trailing `{`
            // does NOT bind to the element (which would mis-build `[_](T{...})`).
            // The `{ ... }` of `[_]T{ ... }` initializes the WHOLE array type, so
            // we build the `ArrayType` first and re-associate the typed-init onto
            // it below.
            let inner = {
                let saved = self.no_struct_lit;
                self.no_struct_lit = true;
                let t = self.parse_unary();
                self.no_struct_lit = saved;
                t
            };
            let span = start.merge(inner.span());
            let array_ty = Expr::ArrayType {
                len: Box::new(len),
                inner: Box::new(inner),
                span,
            };
            // `[N]T{ ... }` — the initializer's type is the full array type.
            if self.at(TokenKind::LBrace) && !self.no_struct_lit {
                let (body, end) = self.parse_init_body();
                let span = array_ty.span().merge(end);
                Expr::Init {
                    ty: Some(Box::new(array_ty)),
                    body,
                    span,
                }
            } else {
                array_ty
            }
        }
    }

    /// Parses pointer/slice qualifiers — `const` and `align(e)` — in any order,
    /// returning whether `const` was present and the optional alignment.
    fn parse_ptr_qualifiers(&mut self) -> (bool, Option<Expr>) {
        let mut is_const = false;
        let mut align = None;
        loop {
            match self.cur_kind() {
                TokenKind::KwConst if !is_const => {
                    self.bump();
                    is_const = true;
                }
                TokenKind::KwAlign if align.is_none() => {
                    align = self.parse_align_clause();
                }
                _ => break,
            }
        }
        (is_const, align)
    }

    /// Parses an optional `align(expr)` clause.
    fn parse_align_clause(&mut self) -> Option<Expr> {
        if self.at(TokenKind::KwAlign) {
            self.bump();
            self.expect(TokenKind::LParen, "after `align`");
            let e = self.parse_expr_no_struct(false);
            self.expect(TokenKind::RParen, "to close `align(...)`");
            Some(e)
        } else {
            None
        }
    }

    /// Level 1: a primary followed by a chain of postfix operators (call, index,
    /// slice, field, `.*`, `.?`, and typed-initializer `T{...}`).
    fn parse_postfix(&mut self) -> Expr {
        let (_guard, over) = self.enter();
        if over {
            return self.too_deep();
        }
        let mut base = self.parse_primary();
        loop {
            // If the depth cap was hit while parsing `base`, stop chaining: the
            // cursor is parked on an unconsumed deep construct (e.g. a `(`), and
            // continuing would wrap it in call/index/init nodes one per token,
            // rebuilding an unbounded tree. Bail out to recovery instead.
            if self.depth_exceeded {
                break;
            }
            match self.cur_kind() {
                TokenKind::LParen => base = self.parse_call(base),
                TokenKind::LBracket => base = self.parse_index_or_slice(base),
                TokenKind::Dot => {
                    self.bump();
                    let field_span = self.here();
                    let name = self.expect_ident_text("after `.`");
                    let span = base.span().merge(field_span);
                    base = Expr::Field {
                        base: Box::new(base),
                        field: name,
                        span,
                    };
                }
                TokenKind::DotStar => {
                    let span = base.span().merge(self.here());
                    self.bump();
                    base = Expr::Deref {
                        base: Box::new(base),
                        span,
                    };
                }
                TokenKind::DotQuestion => {
                    let span = base.span().merge(self.here());
                    self.bump();
                    base = Expr::Unwrap {
                        base: Box::new(base),
                        span,
                    };
                }
                TokenKind::LBrace if !self.no_struct_lit => {
                    // Typed initializer `T{ ... }`.
                    let (body, end) = self.parse_init_body();
                    let span = base.span().merge(end);
                    base = Expr::Init {
                        ty: Some(Box::new(base)),
                        body,
                        span,
                    };
                }
                _ => break,
            }
        }
        base
    }

    /// Parses a call `callee(args...)`.
    fn parse_call(&mut self, callee: Expr) -> Expr {
        self.bump(); // `(`
        let args = self.parse_arg_list(TokenKind::RParen);
        let end = self.expect(TokenKind::RParen, "to close the argument list");
        let span = callee.span().merge(end);
        Expr::Call {
            callee: Box::new(callee),
            args,
            span,
        }
    }

    /// Parses `base[index]` or `base[lo..hi]` / `base[lo..]`.
    fn parse_index_or_slice(&mut self, base: Expr) -> Expr {
        self.bump(); // `[`
                     // Indices/slices are value contexts: a struct literal inside is fine.
        let lo = self.parse_expr_no_struct(false);
        if self.at(TokenKind::DotDot) {
            self.bump();
            let hi = if self.at(TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.parse_expr_no_struct(false)))
            };
            let end = self.expect(TokenKind::RBracket, "to close the slice");
            let span = base.span().merge(end);
            Expr::SliceExpr {
                base: Box::new(base),
                lo: Box::new(lo),
                hi,
                span,
            }
        } else {
            let end = self.expect(TokenKind::RBracket, "to close the index");
            let span = base.span().merge(end);
            Expr::Index {
                base: Box::new(base),
                index: Box::new(lo),
                span,
            }
        }
    }

    /// Parses a comma-separated argument list up to (not consuming) `close`.
    /// Builtin/call arguments may be types or expressions. We parse a full
    /// expression (the binary cascade subsumes the type prefixes `?`/`*`/`[]`),
    /// then apply a trailing `E!T` error-union tail if a bare `!` follows, so a
    /// type argument like `*u32`, `[]const u8`, or `E!T` is accepted.
    fn parse_arg_list(&mut self, close: TokenKind) -> Vec<Expr> {
        let mut args = Vec::new();
        while !self.at(close) && !self.at_eof() {
            let saved = self.no_struct_lit;
            self.no_struct_lit = false;
            let mut arg = self.parse_expr();
            if self.at(TokenKind::Bang) {
                self.bump();
                let ok = self.parse_unary();
                let span = arg.span().merge(ok.span());
                arg = Expr::ErrorUnion {
                    err: Some(Box::new(arg)),
                    ok: Box::new(ok),
                    span,
                };
            }
            args.push(arg);
            self.no_struct_lit = saved;
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        args
    }

    /// Parses a primary expression, dispatching on the current token.
    pub(crate) fn parse_primary(&mut self) -> Expr {
        let (_guard, over) = self.enter();
        if over {
            return self.too_deep();
        }
        let start = self.here();
        match self.cur_kind() {
            TokenKind::IntLiteral => {
                let text = self.cur().text.clone();
                let base = int_base(&text);
                self.bump();
                Expr::Int {
                    text,
                    base,
                    span: start,
                }
            }
            TokenKind::FloatLiteral => {
                let text = self.cur().text.clone();
                self.bump();
                Expr::Float { text, span: start }
            }
            TokenKind::StringLiteral | TokenKind::MultilineString => {
                let text = self.cur().text.clone();
                self.bump();
                Expr::Str { text, span: start }
            }
            TokenKind::CharLiteral => {
                let text = self.cur().text.clone();
                self.bump();
                Expr::Char { text, span: start }
            }
            TokenKind::KwTrue => {
                self.bump();
                Expr::Bool {
                    value: true,
                    span: start,
                }
            }
            TokenKind::KwFalse => {
                self.bump();
                Expr::Bool {
                    value: false,
                    span: start,
                }
            }
            TokenKind::KwNull => {
                self.bump();
                Expr::Null { span: start }
            }
            TokenKind::KwUndefined => {
                self.bump();
                Expr::Undefined { span: start }
            }
            TokenKind::KwUnreachable => {
                self.bump();
                Expr::Unreachable { span: start }
            }
            // A labeled block or loop used as an expression: `name : { ... }`,
            // `name : while (...)`, `name : for (...)`, `name : inline ...`.
            TokenKind::Ident
                if self.peek_kind(1) == TokenKind::Colon
                    && matches!(
                        self.peek_kind(2),
                        TokenKind::LBrace
                            | TokenKind::KwWhile
                            | TokenKind::KwFor
                            | TokenKind::KwInline
                    ) =>
            {
                let label = self.cur().text.clone();
                self.bump(); // name
                self.bump(); // `:`
                match self.cur_kind() {
                    TokenKind::LBrace => {
                        let (body, bspan) = self.parse_block();
                        Expr::Block {
                            label: Some(label),
                            body,
                            span: start.merge(bspan),
                        }
                    }
                    TokenKind::KwWhile => self.parse_while(Some(label), false, true),
                    TokenKind::KwFor => self.parse_for(Some(label), false, true),
                    _ => self.parse_inline_loop(Some(label), true),
                }
            }
            TokenKind::Ident | TokenKind::EscapedIdent => {
                let name = self.cur().text.clone();
                self.bump();
                Expr::Ident { name, span: start }
            }
            TokenKind::Builtin => {
                let name = self.cur().text.clone();
                self.bump();
                self.expect(TokenKind::LParen, "after a builtin name");
                let args = self.parse_arg_list(TokenKind::RParen);
                let end = self.expect(TokenKind::RParen, "to close the builtin call");
                Expr::Builtin {
                    name,
                    args,
                    span: start.merge(end),
                }
            }
            TokenKind::Dot => self.parse_dot_primary(start),
            TokenKind::KwError => self.parse_error_primary(start),
            TokenKind::KwStruct | TokenKind::KwEnum | TokenKind::KwUnion | TokenKind::KwExtern => {
                Expr::Container(Box::new(self.parse_container()))
            }
            TokenKind::KwFn => self.parse_fn_type(),
            TokenKind::LParen => {
                // Grouped expression `( expr )`. The inner is a full expression
                // (so `(a - b)`, `(A || B)`, etc. all parse); a following `!T`
                // at type level is applied by the enclosing `parse_type`, not
                // here. A struct literal inside the parens is fine.
                self.bump();
                let saved = self.no_struct_lit;
                self.no_struct_lit = false;
                let inner = self.parse_expr();
                self.no_struct_lit = saved;
                self.expect(TokenKind::RParen, "to close the grouped expression");
                inner
            }
            TokenKind::LBrace => {
                // A bare block used as an expression.
                let (body, span) = self.parse_block();
                Expr::Block {
                    label: None,
                    body,
                    span,
                }
            }
            TokenKind::KwComptime => {
                // Defensive: comptime reaching primary is a prefix comptime expr.
                self.bump();
                let inner = self.parse_unary();
                let span = start.merge(inner.span());
                Expr::Comptime {
                    inner: Box::new(inner),
                    span,
                }
            }
            TokenKind::KwIf => self.parse_if(true),
            TokenKind::KwSwitch => self.parse_switch(),
            TokenKind::KwWhile => self.parse_while(None, false, true),
            TokenKind::KwFor => self.parse_for(None, false, true),
            TokenKind::KwInline => self.parse_inline_loop(None, true),
            _ => {
                let span = self.here();
                let e = self.error_expr(
                    span,
                    format!("expected an expression, found {}", self.cur_desc()),
                );
                // Ensure progress so callers never loop forever on this token.
                if !self.at_eof() && !self.at(TokenKind::RBrace) && !self.at(TokenKind::RParen) {
                    self.bump();
                }
                e
            }
        }
    }

    /// Parses `inline while` / `inline for` (loop headers preceded by `inline`).
    pub(crate) fn parse_inline_loop(&mut self, label: Option<String>, as_expr: bool) -> Expr {
        self.bump(); // `inline`
        match self.cur_kind() {
            TokenKind::KwWhile => self.parse_while(label, true, as_expr),
            TokenKind::KwFor => self.parse_for(label, true, as_expr),
            _ => {
                let span = self.here();
                self.error_expr(span, "expected `while` or `for` after `inline`")
            }
        }
    }

    /// Parses a `.`-introduced primary: `.Name` (enum literal) or `.{ ... }`
    /// (anonymous init literal).
    fn parse_dot_primary(&mut self, start: Span) -> Expr {
        self.bump(); // `.`
        match self.cur_kind() {
            TokenKind::LBrace => {
                let (body, end) = self.parse_init_body();
                Expr::Init {
                    ty: None,
                    body,
                    span: start.merge(end),
                }
            }
            TokenKind::Ident => {
                let name = self.cur().text.clone();
                let end = self.here();
                self.bump();
                Expr::EnumLiteral {
                    name,
                    span: start.merge(end),
                }
            }
            _ => {
                let span = self.here();
                self.error_expr(span, "expected an identifier or `{` after `.`")
            }
        }
    }

    /// Parses `error.Name` (error literal) or `error { ... }` (error-set type).
    fn parse_error_primary(&mut self, start: Span) -> Expr {
        self.bump(); // `error`
        match self.cur_kind() {
            TokenKind::Dot => {
                self.bump();
                let end = self.here();
                let name = self.expect_ident_text("after `error.`");
                Expr::ErrorLiteral {
                    name,
                    span: start.merge(end),
                }
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(TokenKind::RBrace) && !self.at_eof() {
                    if self.at(TokenKind::Ident) {
                        fields.push(self.cur().text.clone());
                        self.bump();
                    } else {
                        let span = self.here();
                        self.error(span, "expected an error name");
                        if !self.at(TokenKind::Comma) {
                            break;
                        }
                    }
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                let end = self.expect(TokenKind::RBrace, "to close the error set");
                Expr::ErrorSet {
                    fields,
                    span: start.merge(end),
                }
            }
            _ => {
                let span = self.here();
                self.error_expr(span, "expected `.` or `{` after `error`")
            }
        }
    }

    /// Parses an initializer body `{ ... }` (named fields or a positional
    /// tuple), returning the body and the span of the closing `}`. The cursor
    /// must be on the `{`.
    fn parse_init_body(&mut self) -> (InitBody, Span) {
        self.bump(); // `{`
        let saved = self.no_struct_lit;
        self.no_struct_lit = false;
        // Distinguish named (`.f = v`) from tuple form by the leading tokens.
        let is_named = self.at(TokenKind::Dot) && self.peek_kind(1) == TokenKind::Ident;
        let body = if self.at(TokenKind::RBrace) {
            // Empty `.{}` — treat as an empty tuple.
            InitBody::Tuple(Vec::new())
        } else if is_named {
            let mut fields = Vec::new();
            while !self.at(TokenKind::RBrace) && !self.at_eof() {
                let fstart = self.here();
                self.expect(TokenKind::Dot, "before a field name");
                let name = self.expect_ident_text("in a field initializer");
                self.expect(TokenKind::Eq, "after a field name");
                let value = self.parse_expr();
                let span = fstart.merge(value.span());
                fields.push(FieldInit { name, value, span });
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            InitBody::Fields(fields)
        } else {
            let mut elems = Vec::new();
            while !self.at(TokenKind::RBrace) && !self.at_eof() {
                elems.push(self.parse_expr());
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            InitBody::Tuple(elems)
        };
        self.no_struct_lit = saved;
        let end = self.expect(TokenKind::RBrace, "to close the initializer");
        (body, end)
    }

    // ---- shared helpers used across modules ------------------------------

    /// Parses an expression with `no_struct_lit` set to `value` for the
    /// duration, restoring the previous value afterward.
    pub(crate) fn parse_expr_no_struct(&mut self, value: bool) -> Expr {
        let saved = self.no_struct_lit;
        self.no_struct_lit = value;
        let e = self.parse_expr();
        self.no_struct_lit = saved;
        e
    }

    /// Consumes an identifier token and returns its text, or records an error
    /// and returns an empty string (without consuming a non-identifier).
    pub(crate) fn expect_ident_text(&mut self, ctx: &str) -> String {
        if self.at(TokenKind::Ident) || self.at(TokenKind::EscapedIdent) {
            let t = self.cur().text.clone();
            self.bump();
            t
        } else {
            let span = self.here();
            self.error(span, format!("expected an identifier {ctx}"));
            String::new()
        }
    }

    /// Consumes a capture name (`identifier` or `_`); both lex as `Ident`.
    fn expect_capture_name(&mut self) -> String {
        self.expect_ident_text("as a capture name")
    }

    /// Parses a `payload_capture`: `| [*] name {, [*] name} |`. The cursor must
    /// be on the opening `|`.
    pub(crate) fn parse_capture(&mut self) -> Capture {
        let start = self.expect(TokenKind::Pipe, "to open a capture");
        let mut names = Vec::new();
        while !self.at(TokenKind::Pipe) && !self.at_eof() {
            names.push(self.parse_one_capture());
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let end = self.expect(TokenKind::Pipe, "to close a capture");
        Capture {
            names,
            span: start.merge(end),
        }
    }

    /// Parses one `[*] name` capture name.
    pub(crate) fn parse_one_capture(&mut self) -> CaptureName {
        let start = self.here();
        let by_ref = self.eat(TokenKind::Star).is_some();
        let name_span = self.here();
        let name = self.expect_capture_name();
        CaptureName {
            by_ref,
            name,
            span: start.merge(name_span),
        }
    }

    /// Parses the `for` operands: `for_operand {, for_operand}` where each
    /// operand is `expr` or `expr .. [expr]`. The cursor is just past `(`.
    pub(crate) fn parse_for_operands(&mut self) -> Vec<ForOperand> {
        let mut operands = Vec::new();
        loop {
            let lo = self.parse_expr();
            if self.at(TokenKind::DotDot) {
                let dotdot = self.bump();
                let (hi, span) = if self.at(TokenKind::Comma) || self.at(TokenKind::RParen) {
                    (None, lo.span().merge(dotdot))
                } else {
                    let hi = self.parse_expr();
                    let span = lo.span().merge(hi.span());
                    (Some(Box::new(hi)), span)
                };
                operands.push(ForOperand::Range {
                    lo: Box::new(lo),
                    hi,
                    span,
                });
            } else {
                operands.push(ForOperand::Value(lo));
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        operands
    }
}

/// Maps a comparison token to its [`BinOp`]; `None` for non-comparators.
fn compare_op(k: TokenKind) -> Option<BinOp> {
    use TokenKind as T;
    Some(match k {
        T::EqEq => BinOp::Eq,
        T::NotEq => BinOp::Ne,
        T::Lt => BinOp::Lt,
        T::LtEq => BinOp::Le,
        T::Gt => BinOp::Gt,
        T::GtEq => BinOp::Ge,
        _ => return None,
    })
}

/// Determines an integer literal's [`IntBase`](k2_syntax::IntBase) from its
/// radix prefix.
fn int_base(text: &str) -> k2_syntax::IntBase {
    use k2_syntax::IntBase;
    let t = text.as_bytes();
    if t.len() >= 2 && t[0] == b'0' {
        match t[1] {
            b'x' | b'X' => return IntBase::Hex,
            b'o' | b'O' => return IntBase::Oct,
            b'b' | b'B' => return IntBase::Bin,
            _ => {}
        }
    }
    IntBase::Dec
}
