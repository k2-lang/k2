//! Type parsing: the `parse_type` wrapper, container bodies, and `fn` types.
//!
//! k2 types are expressions, so the prefix type-constructors (`?`, `*`, `[]`,
//! `[N]`, `!T`) live in [`Parser::parse_unary`](crate::Parser::parse_unary).
//! This module adds the two pieces that only make sense in *type position*: the
//! infix `E!T` error-union (handled by [`Parser::parse_type`]), and the
//! container-body / fn-type / param machinery shared between declarations and
//! type expressions.

use k2_lexer::TokenKind;
use k2_syntax::{Container, ContainerKind, Expr, Field, Member, Param, Span, UnionTag};

use crate::Parser;

impl Parser {
    /// Parses a type. This is `parse_unary` (which handles the prefix
    /// constructors) followed by the infix `E!T` error-union tail, which only
    /// applies in type position.
    pub(crate) fn parse_type(&mut self) -> Expr {
        let (_guard, over) = self.enter();
        if over {
            return self.too_deep();
        }
        let lhs = self.parse_unary();
        self.parse_type_tail(lhs)
    }

    /// Parses a type with `no_struct_lit` set, so a trailing `{` is not read as
    /// a typed-init (`T{...}`) — it belongs to a following block. Used for a
    /// function's return type, where the `{` opens the body. The previous flag
    /// is restored afterward.
    pub(crate) fn parse_type_no_struct(&mut self) -> Expr {
        let saved = self.no_struct_lit;
        self.no_struct_lit = true;
        let t = self.parse_type();
        self.no_struct_lit = saved;
        t
    }

    /// Applies the infix `E!T` error-union form if a bare `!` follows `lhs`.
    fn parse_type_tail(&mut self, lhs: Expr) -> Expr {
        if self.at(TokenKind::Bang) {
            self.bump();
            let ok = self.parse_unary();
            let span = lhs.span().merge(ok.span());
            Expr::ErrorUnion {
                err: Some(Box::new(lhs)),
                ok: Box::new(ok),
                span,
            }
        } else {
            lhs
        }
    }

    /// Parses a container *type* expression: `[extern] struct {...}`,
    /// `enum(tag?) {...}`, or `union(enum|type)? {...}`. The cursor is on
    /// `extern`/`struct`/`enum`/`union`.
    pub(crate) fn parse_container(&mut self) -> Container {
        let (_guard, over) = self.enter();
        if over {
            // Past the recursion cap: emit the single "nesting too deep"
            // diagnostic and return an empty struct container without descending
            // into the (deeply nested) body.
            let _ = self.too_deep();
            return Container {
                kind: ContainerKind::Struct {
                    is_extern: false,
                    is_packed: false,
                },
                members: Vec::new(),
                span: self.here(),
            };
        }
        let start = self.here();
        let is_extern = self.eat(TokenKind::KwExtern).is_some();
        // `packed` is a CONTEXTUAL qualifier (not a reserved keyword), accepted
        // here only directly before `struct` — mirroring how `anytype` is a
        // contextual identifier. `packed struct {...}` requests the LSB-first
        // bit-packed layout (spec §02).
        let is_packed = !is_extern
            && self.at(TokenKind::Ident)
            && self.cur().text == "packed"
            && self.peek_kind(1) == TokenKind::KwStruct;
        if is_packed {
            self.bump(); // `packed`
        }
        let kind = match self.cur_kind() {
            TokenKind::KwStruct => {
                self.bump();
                ContainerKind::Struct {
                    is_extern,
                    is_packed,
                }
            }
            TokenKind::KwEnum => {
                self.bump();
                let tag = if self.at(TokenKind::LParen) {
                    self.bump();
                    let t = self.parse_type();
                    self.expect(TokenKind::RParen, "to close the enum tag type");
                    Some(Box::new(t))
                } else {
                    None
                };
                ContainerKind::Enum { tag }
            }
            TokenKind::KwUnion => {
                self.bump();
                let tag = if self.at(TokenKind::LParen) {
                    self.bump();
                    let t = if self.at(TokenKind::KwEnum) {
                        self.bump();
                        UnionTag::Inferred
                    } else {
                        UnionTag::Typed(Box::new(self.parse_type()))
                    };
                    self.expect(TokenKind::RParen, "to close the union tag");
                    t
                } else {
                    UnionTag::None
                };
                ContainerKind::Union { tag }
            }
            _ => {
                let span = self.here();
                self.error(span, "expected `struct`, `enum`, or `union`");
                ContainerKind::Struct {
                    is_extern,
                    is_packed,
                }
            }
        };

        self.expect(TokenKind::LBrace, "to open a container body");
        let is_enum = matches!(kind, ContainerKind::Enum { .. });
        let mut members = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            let before = self.pos;
            if let Some(m) = self.parse_member(is_enum) {
                members.push(m);
            }
            // Loop guard: never spin on a member that consumed nothing.
            if self.pos == before {
                self.bump();
            }
        }
        let end = self.expect(TokenKind::RBrace, "to close a container body");
        Container {
            kind,
            members,
            span: start.merge(end),
        }
    }

    /// Parses one container member: a field or a nested declaration. Returns
    /// `None` only if recovery skipped the member entirely.
    ///
    /// Reconciliation (defect 6): struct/enum/union bodies deliberately share
    /// this ONE uniform member parser, so the nested-declaration set (`const`,
    /// `var`, `fn`, `test`, `comptime`) is identical across containers; only the
    /// FIELD shape differs (enum fields vs. `name : type` fields). Rather than
    /// hand-narrow this per container (higher risk of regressing examples), the
    /// grammar (`docs/grammar.ebnf`, `enum_member`/`union_member`) was made
    /// co-normative with this uniform behavior via a shared `container_decl`.
    fn parse_member(&mut self, is_enum: bool) -> Option<Member> {
        let (_guard, over) = self.enter();
        if over {
            // Too deep: record the single diagnostic and skip this member so the
            // container body loop terminates without further descent.
            let _ = self.too_deep();
            if !self.at_eof() && !self.at(TokenKind::RBrace) {
                self.bump();
            }
            return None;
        }
        let doc = self.take_doc();
        // Nested declarations begin with a decl keyword (after optional `pub`).
        let (after_pub, pub_off) = if self.at(TokenKind::KwPub) {
            (self.peek_kind(1), 1)
        } else {
            (self.cur_kind(), 0)
        };
        let is_decl = match after_pub {
            TokenKind::KwConst
            | TokenKind::KwVar
            | TokenKind::KwFn
            | TokenKind::KwTest
            | TokenKind::KwExtern
            | TokenKind::KwExport
            | TokenKind::KwInline => true,
            // `comptime { ... }` / `comptime var` / `comptime const` are decls;
            // `comptime name : type` is a comptime *field*.
            TokenKind::KwComptime => matches!(
                self.peek_kind(pub_off + 1),
                TokenKind::LBrace | TokenKind::KwVar | TokenKind::KwConst
            ),
            _ => false,
        };

        if is_decl {
            let item = self.parse_item_inner(doc)?;
            return Some(Member::Decl(item));
        }

        // Otherwise a field.
        if is_enum {
            Some(Member::Field(self.parse_enum_field(doc)))
        } else {
            Some(Member::Field(self.parse_struct_field(doc)))
        }
    }

    /// Parses a struct/union field:
    /// `[pub] [comptime] name : type [align(e)] [= default] ,`.
    fn parse_struct_field(&mut self, doc: Option<String>) -> Field {
        let start = self.here();
        let is_pub = self.eat(TokenKind::KwPub).is_some();
        let is_comptime = self.eat(TokenKind::KwComptime).is_some();
        let name = self.expect_ident_text("as a field name");
        self.expect(TokenKind::Colon, "after a field name");
        let ty = self.parse_type();
        let align = if self.at(TokenKind::KwAlign) {
            self.bump();
            self.expect(TokenKind::LParen, "after `align`");
            let e = self.parse_expr_no_struct(false);
            self.expect(TokenKind::RParen, "to close `align(...)`");
            Some(e)
        } else {
            None
        };
        let default = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_expr())
        } else {
            None
        };
        let end = self.eat_field_terminator("after a field");
        Field {
            doc,
            is_pub,
            is_comptime,
            name,
            ty: Some(ty),
            align,
            default,
            span: start.merge(end),
        }
    }

    /// Consumes a field terminator: a `,`, or nothing when the next token is the
    /// closing `}` (a trailing comma on the last field is optional). Returns the
    /// span of the terminator (or the previous token if omitted).
    fn eat_field_terminator(&mut self, ctx: &str) -> Span {
        if let Some(c) = self.eat(TokenKind::Comma) {
            c
        } else if self.at(TokenKind::RBrace) {
            self.here()
        } else {
            self.expect(TokenKind::Comma, ctx)
        }
    }

    /// Parses an enum field: `name [= value] ,`.
    fn parse_enum_field(&mut self, doc: Option<String>) -> Field {
        let start = self.here();
        let name = self.expect_ident_text("as an enum field name");
        let default = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_expr())
        } else {
            None
        };
        let end = self.eat_field_terminator("after an enum field");
        Field {
            doc,
            is_pub: false,
            is_comptime: false,
            name,
            ty: None,
            align: None,
            default,
            span: start.merge(end),
        }
    }

    /// Parses a `fn` *type* expression: `fn (params) Ret`. The cursor is on
    /// `fn`.
    pub(crate) fn parse_fn_type(&mut self) -> Expr {
        let start = self.here();
        self.bump(); // `fn`
        self.expect(TokenKind::LParen, "after `fn` in a function type");
        let (params, is_varargs) = self.parse_fn_type_params();
        self.expect(TokenKind::RParen, "to close the function-type parameters");
        let ret = self.parse_type();
        let span = start.merge(ret.span());
        Expr::FnType {
            params,
            is_varargs,
            ret: Box::new(ret),
            span,
        }
    }

    /// Parses fn-*type* parameters: `[comptime] [name :] (type | anytype)`,
    /// or `...` for a C-variadic. Returns the params and a varargs flag. The
    /// cursor is just past `(`.
    fn parse_fn_type_params(&mut self) -> (Vec<Param>, bool) {
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
            // Optional `name :` — only when `Ident` is followed by `:`.
            let name = if (self.at(TokenKind::Ident) || self.at(TokenKind::EscapedIdent))
                && self.peek_kind(1) == TokenKind::Colon
            {
                let n = self.cur().text.clone();
                self.bump(); // name
                self.bump(); // `:`
                n
            } else {
                String::new()
            };
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

    /// Parses a parameter type, accepting the predeclared `anytype` marker.
    pub(crate) fn parse_param_type(&mut self) -> Expr {
        if self.at(TokenKind::Ident) && self.cur().text == "anytype" {
            let span = self.here();
            self.bump();
            Expr::AnyType { span }
        } else {
            self.parse_type()
        }
    }
}
