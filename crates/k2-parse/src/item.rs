//! Top-level (and nested) item parsing: declarations, function signatures,
//! parameters, tests, and the source-file entry point.
//!
//! Container-type declarations are *not* a separate item kind — a
//! `const T = struct {...};` is an [`Item::Const`] whose value is an
//! [`Expr::Container`](k2_syntax::Expr::Container). The same `parse_item_inner`
//! drives both file-scope items and the nested declarations inside a container
//! body (reached via [`Parser::parse_container`](crate::Parser::parse_container)).

use k2_lexer::TokenKind;
use k2_syntax::{Item, Param, SourceFile, Span};

use crate::Parser;

impl Parser {
    /// Parses a whole source file: leading file-level doc comments, then the
    /// top-level items.
    pub(crate) fn parse_source_file(&mut self) -> SourceFile {
        // File-level doc comments only attach to the file when no declaration
        // follows on the next non-doc token would otherwise claim them. The
        // grammar lists `{ doc_comment }` before any item; we collect a leading
        // run, but if an item follows immediately we hand them to that item.
        let mut doc: Vec<String> = Vec::new();
        let mut items = Vec::new();
        loop {
            // Collect a run of doc comments.
            let mut pending: Vec<String> = Vec::new();
            while self.at(TokenKind::DocComment) {
                pending.push(self.cur().text.clone());
                self.bump();
            }
            if self.at_eof() {
                // Trailing doc comments with no item: treat as file-level.
                doc.extend(pending);
                break;
            }
            let item_doc = if pending.is_empty() {
                None
            } else {
                Some(pending.join("\n"))
            };
            let before = self.pos;
            match self.parse_item_inner(item_doc) {
                Some(item) => items.push(item),
                None => self.synchronize_item(),
            }
            if self.pos == before {
                // Loop guard: force progress on a stuck item.
                self.synchronize_item();
            }
            if self.at_eof() {
                break;
            }
        }
        SourceFile { doc, items }
    }

    /// Parses one item given an already-consumed doc comment. Collects fn
    /// qualifiers, then dispatches on the declaration keyword. Returns `None`
    /// to request item-level recovery.
    pub(crate) fn parse_item_inner(&mut self, doc: Option<String>) -> Option<Item> {
        let start = self.here();
        let is_pub = self.eat(TokenKind::KwPub).is_some();

        // `extern` / `export` / `inline` qualifiers precede `fn` (in any order).
        let mut is_extern = false;
        let mut is_export = false;
        let mut is_inline = false;
        loop {
            match self.cur_kind() {
                TokenKind::KwExtern => {
                    // `extern` before `struct` is a container, not a fn qualifier.
                    if self.peek_kind(1) == TokenKind::KwStruct {
                        break;
                    }
                    is_extern = true;
                    self.bump();
                }
                TokenKind::KwExport => {
                    is_export = true;
                    self.bump();
                }
                TokenKind::KwInline => {
                    is_inline = true;
                    self.bump();
                }
                _ => break,
            }
        }

        match self.cur_kind() {
            TokenKind::KwConst => Some(self.parse_const_item(doc, is_pub, start)),
            TokenKind::KwVar => Some(self.parse_var_item(doc, is_pub, start)),
            TokenKind::KwFn => {
                Some(self.parse_fn_item(doc, is_pub, is_extern, is_export, is_inline, start))
            }
            TokenKind::KwTest => Some(self.parse_test_item(doc, start)),
            TokenKind::KwComptime => Some(self.parse_comptime_item(start)),
            _ => {
                let span = self.here();
                self.error(
                    span,
                    format!("expected a declaration, found {}", self.cur_desc()),
                );
                None
            }
        }
    }

    /// Parses `const name [: type] = value;`.
    fn parse_const_item(&mut self, doc: Option<String>, is_pub: bool, start: Span) -> Item {
        self.bump(); // `const`
        let name = self.expect_ident_text("after `const`");
        let ty = self.parse_opt_type_annotation();
        self.expect(TokenKind::Eq, "in a `const` declaration");
        let value = self.parse_expr();
        let end = self.expect(TokenKind::Semicolon, "after a `const` declaration");
        Item::Const {
            doc,
            is_pub,
            name,
            ty,
            value,
            span: start.merge(end),
        }
    }

    /// Parses `var name [: type] [= value];`.
    fn parse_var_item(&mut self, doc: Option<String>, is_pub: bool, start: Span) -> Item {
        self.bump(); // `var`
        let name = self.expect_ident_text("after `var`");
        let ty = self.parse_opt_type_annotation();
        let value = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_expr())
        } else {
            None
        };
        let end = self.expect(TokenKind::Semicolon, "after a `var` declaration");
        Item::Var {
            doc,
            is_pub,
            name,
            ty,
            value,
            span: start.merge(end),
        }
    }

    /// Parses a function declaration:
    /// `fn name (params) [align(e)] return_type (block | ;)`.
    fn parse_fn_item(
        &mut self,
        doc: Option<String>,
        is_pub: bool,
        is_extern: bool,
        is_export: bool,
        is_inline: bool,
        start: Span,
    ) -> Item {
        self.bump(); // `fn`
        let name = self.expect_ident_text("as a function name");
        self.expect(TokenKind::LParen, "to open the parameter list");
        let (params, is_varargs) = self.parse_param_list();
        self.expect(TokenKind::RParen, "to close the parameter list");

        let align = if self.at(TokenKind::KwAlign) {
            self.bump();
            self.expect(TokenKind::LParen, "after `align`");
            let e = self.parse_expr_no_struct(false);
            self.expect(TokenKind::RParen, "to close `align(...)`");
            Some(e)
        } else {
            None
        };

        // The return type is followed by either the body `{` or `;`, so a
        // trailing `{` must not be read as a `T{...}` initializer.
        let ret = self.parse_type_no_struct();

        let (body, end) = if self.at(TokenKind::LBrace) {
            let (stmts, bspan) = self.parse_block();
            (Some(stmts), bspan)
        } else {
            let semi = self.expect(TokenKind::Semicolon, "after an `extern` prototype");
            (None, semi)
        };

        Item::Fn {
            doc,
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
            span: start.merge(end),
        }
    }

    /// Parses a `fn`-declaration parameter list: `param {, param} [,]`, where a
    /// `param` is `[comptime] (name | _) : (type | anytype)` or `...`
    /// (C-variadic). The cursor is just past `(`.
    fn parse_param_list(&mut self) -> (Vec<Param>, bool) {
        let mut params = Vec::new();
        let mut is_varargs = false;
        while !self.at(TokenKind::RParen) && !self.at_eof() {
            if self.at(TokenKind::DotDotDot) {
                self.bump();
                is_varargs = true;
                let _ = self.eat(TokenKind::Comma);
                break;
            }
            let start = self.here();
            let is_comptime = self.eat(TokenKind::KwComptime).is_some();
            let name = self.expect_ident_text("as a parameter name");
            self.expect(TokenKind::Colon, "after a parameter name");
            let ty = self.parse_param_type();
            let span = start.merge(ty.span());
            params.push(Param {
                is_comptime,
                name,
                ty,
                span,
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        (params, is_varargs)
    }

    /// Parses `test [string | ident] { ... }`.
    fn parse_test_item(&mut self, doc: Option<String>, start: Span) -> Item {
        self.bump(); // `test`
        let name = match self.cur_kind() {
            TokenKind::StringLiteral | TokenKind::Ident => {
                let t = self.cur().text.clone();
                self.bump();
                Some(t)
            }
            _ => None,
        };
        let (body, bspan) = self.parse_block();
        Item::Test {
            doc,
            name,
            body,
            span: start.merge(bspan),
        }
    }

    /// Parses a top-level `comptime { ... }` block.
    fn parse_comptime_item(&mut self, start: Span) -> Item {
        self.bump(); // `comptime`
        let (body, bspan) = self.parse_block();
        Item::Comptime {
            body,
            span: start.merge(bspan),
        }
    }
}
