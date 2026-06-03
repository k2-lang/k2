//! The canonical printer: turns a parsed [`SourceFile`] (plus recovered line
//! comments) into k2's one obvious source form.
//!
//! ## Strategy
//!
//! The printer is a *measure-then-emit* engine. Every expression has a **flat**
//! rendering (a single line, produced by [`Printer::flat_expr`]) that obeys all
//! the spacing rules. At each *decision point* — a function/call/init/error-set
//! parameter list, a container or `switch` body, a brace-less control-flow body
//! — the printer measures the flat form against the 100-column print width and
//! either emits it flat or **breaks** it one element per line. Breaking is
//! all-or-nothing per list and decided innermost-first: a child that must break
//! injects a newline into its flat string, which makes the parent's fit test
//! fail, so the break propagates outward with no backtracking. The result is a
//! pure function of the AST, the comments, and the width.
//!
//! ## Parenthesization
//!
//! The AST has already dropped grouping parentheses, so the printer re-inserts
//! exactly the parentheses precedence and associativity require (see
//! [`prec`]/[`needs_parens`]) and never more. Redundant author parens are not
//! round-tripped; the AST round-trip test certifies this preserves semantics.

use k2_syntax::{
    BinOp, Capture, CaptureName, Container, ContainerKind, Expr, Field, ForOperand, InitBody, Item,
    Member, Param, SourceFile, Stmt, SwitchArm, SwitchItem, SwitchPattern, UnOp, UnionTag,
};

use crate::comments::Comment;

/// The canonical print width (in Unicode scalars). The `.editorconfig` sets no
/// `max_line_length` for `.k2`; we adopt 100 by fiat, matching the repo's Rust
/// limit, and document it here as the single authority for the wrapping engine.
const WIDTH: usize = 100;

/// One indentation level: four spaces, never a tab (spec / `.editorconfig`).
const INDENT: &str = "    ";

/// The stateful canonical printer over one source file.
pub(crate) struct Printer<'a> {
    /// The accumulating output text.
    out: String,
    /// The source, as a scalar vector, used to disambiguate brace-less vs.
    /// braced single-statement control-flow bodies (the AST does not record
    /// whether braces were present, so we peek at the original `{`).
    src: &'a [char],
    /// Current indentation depth (in levels, not spaces).
    depth: usize,
    /// Recovered line comments, in source order.
    comments: &'a [Comment],
    /// Cursor into `comments`: the index of the next comment not yet emitted.
    cursor: usize,
    /// The 1-based source line of the most recently emitted leading/dangling
    /// comment, used to preserve a single author blank line between a comment
    /// block and the node it leads.
    last_comment_line: Option<u32>,
}

impl<'a> Printer<'a> {
    /// Creates a printer for `file`, drawing line comments from `comments` and
    /// brace-presence facts from `src` (a scalar view of the original source).
    pub(crate) fn new(
        _file: &'a SourceFile,
        comments: &'a [Comment],
        src: &'a [char],
    ) -> Printer<'a> {
        Printer {
            out: String::new(),
            src,
            depth: 0,
            comments,
            cursor: 0,
            last_comment_line: None,
        }
    }

    /// Returns `true` if the source character at scalar offset `at` is `{`, i.e.
    /// the node at that span actually opened with a brace. Used to keep a braced
    /// single-statement control-flow body braced (the AST cannot tell brace-less
    /// from braced).
    fn opened_with_brace(&self, at: u32) -> bool {
        self.src.get(at as usize) == Some(&'{')
    }

    /// Returns `true` if the keyword `comptime` immediately precedes the source
    /// position `at` (skipping intervening whitespace). The AST drops the
    /// `comptime` qualifier on a `comptime var`/`comptime const` *statement*, so
    /// the formatter recovers it from the source to stay lossless.
    fn comptime_precedes(&self, at: u32) -> bool {
        let mut i = at as usize;
        // Skip backwards over whitespace.
        while i > 0 {
            match self.src[i - 1] {
                ' ' | '\t' | '\n' | '\r' => i -= 1,
                _ => break,
            }
        }
        // Check the eight characters ending at `i` spell `comptime`, preceded by
        // a non-identifier boundary.
        const KW: &[char] = &['c', 'o', 'm', 'p', 't', 'i', 'm', 'e'];
        if i < KW.len() {
            return false;
        }
        let start = i - KW.len();
        if self.src[start..i] != *KW {
            return false;
        }
        // Ensure it is a whole word (not e.g. `xcomptime`).
        start == 0 || !is_ident_char(self.src[start - 1])
    }

    /// Formats the whole file and returns the canonical text (always ending in a
    /// single `\n`).
    pub(crate) fn print_file(mut self, file: &SourceFile) -> String {
        // File-level doc comments, then one blank line before the first item.
        if !file.doc.is_empty() {
            for entry in &file.doc {
                for line in doc_lines(entry) {
                    self.line(&line);
                }
            }
            self.blank();
        }

        for (i, item) in file.items.iter().enumerate() {
            let item_start = item.span().start;
            // Between items the formatter always forces a single visual gap. A
            // leading comment block carries its own blank-above flag, so emit the
            // separating blank only when no leading comment will.
            if i > 0 && !self.has_comment_before(item_start) {
                self.blank();
            }
            self.emit_leading_comments(item_start);
            self.blank_after_leading(item.span().line);
            self.print_item(item);
        }

        // Any comments after the last item (or in a file with no items).
        self.emit_trailing_file_comments();

        self.finish()
    }

    /// Normalizes and returns the final output: collapse >1 blank line, strip
    /// trailing whitespace, ensure exactly one final newline.
    fn finish(self) -> String {
        let mut result = String::new();
        let mut blanks = 0usize;
        let mut started = false;
        for raw in self.out.lines() {
            let line = raw.trim_end();
            if line.is_empty() {
                blanks += 1;
                continue;
            }
            if started {
                // Collapse any run of blank lines to exactly one.
                if blanks > 0 {
                    result.push('\n');
                }
            }
            result.push_str(line);
            result.push('\n');
            blanks = 0;
            started = true;
        }
        if result.is_empty() {
            // An empty file formats to a single newline.
            result.push('\n');
        }
        result
    }

    // ---- low-level output -------------------------------------------------

    /// The current indentation prefix.
    fn pad(&self) -> String {
        INDENT.repeat(self.depth)
    }

    /// Emits one full line at the current indent (text plus a newline).
    fn line(&mut self, text: &str) {
        let pad = self.pad();
        self.out.push_str(&pad);
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// Emits a blank line (the `finish` pass collapses runs to one).
    fn blank(&mut self) {
        self.out.push('\n');
    }

    // ---- comment handling -------------------------------------------------

    /// Returns `true` if a not-yet-emitted comment starts before `offset`.
    fn has_comment_before(&self, offset: u32) -> bool {
        self.comments
            .get(self.cursor)
            .is_some_and(|c| c.start < offset)
    }

    /// Emits every own-line comment whose start precedes `offset`, at the
    /// current indent. Returns `true` if at least one comment was emitted (so the
    /// caller can suppress a redundant separating blank line). A blank line the
    /// author placed above a comment is preserved (collapsed to one).
    fn emit_leading_comments(&mut self, offset: u32) -> bool {
        let mut emitted = false;
        while let Some(c) = self.comments.get(self.cursor) {
            if c.start >= offset || !c.own_line {
                break;
            }
            if c.blank_before && !self.out.is_empty() {
                self.blank();
            }
            let text = c.text.clone();
            let line = c.line;
            self.line(&text);
            self.last_comment_line = Some(line);
            self.cursor += 1;
            emitted = true;
        }
        emitted
    }

    /// Preserves a single author blank line between the most recently emitted
    /// leading comment block and a node that begins on `node_line`. Called by
    /// item/statement/member printers right before they emit the node.
    fn blank_after_leading(&mut self, node_line: u32) {
        if let Some(cl) = self.last_comment_line.take() {
            if node_line > cl + 1 {
                self.blank();
            }
        }
    }

    /// Consumes and returns a trailing same-line comment for a node whose last
    /// token ends at `end_offset` on line `end_line`, if any. The returned string
    /// is the comment text (without the two-space separator).
    fn take_trailing_comment(&mut self, end_offset: u32, end_line: u32) -> Option<String> {
        self.take_trailing_comment_bounded(end_offset, end_line, u32::MAX)
    }

    /// Like [`Printer::take_trailing_comment`] but only claims the comment when it
    /// starts strictly before `next_bound` (the next sibling element's start, or
    /// the closing delimiter's offset). This stops the *first* of several
    /// same-line elements from greedily claiming a trailing comment that actually
    /// trails a later element on that line.
    fn take_trailing_comment_bounded(
        &mut self,
        end_offset: u32,
        end_line: u32,
        next_bound: u32,
    ) -> Option<String> {
        let c = self.comments.get(self.cursor)?;
        if !c.own_line && c.line == end_line && c.start >= end_offset && c.start < next_bound {
            let text = c.text.clone();
            self.cursor += 1;
            Some(text)
        } else {
            None
        }
    }

    /// Emits any comments that occur before `before_offset` as dangling/leading
    /// comments at the current indent (used inside blocks before a closing `}`).
    fn emit_dangling_comments(&mut self, before_offset: u32) -> bool {
        let mut emitted = false;
        while let Some(c) = self.comments.get(self.cursor) {
            if c.start >= before_offset {
                break;
            }
            if !c.own_line {
                // A stray trailing comment with no owner: emit on its own line so
                // it is never dropped.
                let text = c.text.clone();
                self.line(&text);
                self.cursor += 1;
                emitted = true;
                continue;
            }
            if c.blank_before && emitted {
                self.blank();
            }
            let text = c.text.clone();
            self.line(&text);
            self.cursor += 1;
            emitted = true;
        }
        emitted
    }

    /// Emits whatever comments remain after the last item, at column 0.
    fn emit_trailing_file_comments(&mut self) {
        let mut first = true;
        while let Some(c) = self.comments.get(self.cursor) {
            if c.blank_before && (!first || !self.out.is_empty()) {
                self.blank();
            }
            let text = c.text.clone();
            self.line(&text);
            self.cursor += 1;
            first = false;
        }
    }

    // ---- items ------------------------------------------------------------

    /// Prints one top-level (or nested) item, preceded by its doc comment.
    fn print_item(&mut self, item: &Item) {
        if let Some(doc) = item_doc(item) {
            for line in doc_lines(doc) {
                self.line(&line);
            }
        }
        match item {
            Item::Const {
                is_pub,
                name,
                ty,
                value,
                ..
            } => self.print_const_like("const", *is_pub, name, ty.as_ref(), Some(value), item),
            Item::Var {
                is_pub,
                name,
                ty,
                value,
                ..
            } => self.print_const_like("var", *is_pub, name, ty.as_ref(), value.as_ref(), item),
            Item::Fn { .. } => self.print_fn(item),
            Item::Test {
                name, body, span, ..
            } => {
                let header = match name {
                    Some(n) => format!("test {n} {{"),
                    None => "test {".to_string(),
                };
                self.print_block_construct(&header, body, span.end);
            }
            Item::Comptime { body, span } => {
                self.print_block_construct("comptime {", body, span.end);
            }
        }
    }

    /// Prints a `const`/`var` declaration (item form). A container value is
    /// printed as a multi-line body; everything else uses the value's flat/broken
    /// rendering, terminated by `;`.
    fn print_const_like(
        &mut self,
        kw: &str,
        is_pub: bool,
        name: &str,
        ty: Option<&Expr>,
        value: Option<&Expr>,
        item: &Item,
    ) {
        let mut head = String::new();
        if is_pub {
            head.push_str("pub ");
        }
        head.push_str(kw);
        head.push(' ');
        head.push_str(name);
        if let Some(t) = ty {
            head.push_str(": ");
            head.push_str(&self.flat_expr(t, 0));
        }
        match value {
            Some(v) => {
                head.push_str(" = ");
                self.print_assigned_value(&head, v, ";", item.span().end);
            }
            None => {
                head.push(';');
                self.line(&head);
                self.attach_trailing(item.span().end, item.span().line);
            }
        }
    }

    /// Emits `head` followed by the rendering of `value` and a `tail` (e.g. `;`).
    /// If `value` is a container/`switch`/control-flow/init/call that should be
    /// broken, the appropriate multi-line form is used; otherwise the flat form
    /// is emitted on one line.
    fn print_assigned_value(&mut self, head: &str, value: &Expr, tail: &str, end_offset: u32) {
        // Container values print as a body opened on the `head` line.
        if let Expr::Container(c) = value {
            self.print_container(head, c, tail, end_offset);
            return;
        }
        let col = self.depth * INDENT.len() + head.chars().count();
        let flat = self.flat_expr(value, col);
        // An interior line comment forces the broken form, so the comment has a
        // place to live and is never dropped.
        let has_interior_comment = self.has_comment_before(value.span().end);
        if col + flat.chars().count() + tail.chars().count() <= WIDTH
            && !flat.contains('\n')
            && !has_interior_comment
        {
            let mut s = String::from(head);
            s.push_str(&flat);
            s.push_str(tail);
            self.line(&s);
            self.attach_trailing(end_offset, line_of(value));
        } else {
            self.print_broken_value(head, value, tail, end_offset);
        }
    }

    /// Prints a value that did not fit flat, breaking its outermost delimited
    /// construct. Falls back to the flat form (over width) only when the value
    /// has no breakable structure.
    fn print_broken_value(&mut self, head: &str, value: &Expr, tail: &str, end_offset: u32) {
        match value {
            Expr::Init { ty, body, .. } => {
                self.print_init_broken(head, ty.as_deref(), body, tail, end_offset);
            }
            Expr::Call { callee, args, .. } => {
                let callee_flat = self.flat_expr(callee, 0);
                self.print_call_broken(head, &callee_flat, args, tail, end_offset);
            }
            Expr::Builtin { name, args, .. } => {
                self.print_call_broken(head, name, args, tail, end_offset);
            }
            Expr::If { .. } | Expr::Switch { .. } | Expr::While { .. } | Expr::For { .. } => {
                // Control-flow values: emit `head` then the construct as a body.
                self.print_cf_value(head, value, tail, end_offset);
            }
            Expr::Block { .. } => {
                // A block expression in value position (`const x = blk: { ... };`
                // or a labeled-block statement). The flat form of a non-empty
                // block always contains a forced newline, so it lands here; route
                // it through the shared control-flow printer, which opens the
                // (label-preserving) `{` on the head line, prints the statements,
                // and appends `tail` — without this arm the body would be dropped
                // by the `_` flat fallback (code loss).
                self.print_cf_value(head, value, tail, end_offset);
            }
            Expr::Catch {
                lhs, capture, rhs, ..
            } if is_block_expr(rhs) => {
                // `lhs catch [|cap|] { ... }` — open the block on the head line.
                let cap = match capture {
                    Some(c) => format!("|{c}| "),
                    None => String::new(),
                };
                let opener = format!(
                    "{head}{} catch {cap}",
                    self.flat_child(lhs, value, Side::Left)
                );
                self.print_cf_value(&opener, rhs, tail, end_offset);
            }
            value if is_flat_binary_chain(value) => {
                // A long binary chain in value position wraps at its operators,
                // the same greedy form used for a chain that is a call's sole
                // argument (`print_call_broken`). Keeping the two contexts
                // consistent avoids an over-width top-level line.
                self.print_binary_wrapped(head, value, tail);
                self.attach_trailing(end_offset, line_of(value));
            }
            _ => {
                // No breakable structure: emit flat, accepting the over-width
                // line — there is no canonical wrap for it.
                let flat = self.flat_expr(value, 0);
                let mut s = String::from(head);
                s.push_str(&flat);
                s.push_str(tail);
                self.line(&s);
                self.attach_trailing(end_offset, line_of(value));
            }
        }
    }

    /// Prints a function item: signature (flat or broken params) plus body.
    fn print_fn(&mut self, item: &Item) {
        let Item::Fn {
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
            span,
            ..
        } = item
        else {
            unreachable!("print_fn called on a non-fn item");
        };

        let mut prefix = String::new();
        if *is_pub {
            prefix.push_str("pub ");
        }
        if *is_extern {
            prefix.push_str("extern ");
        }
        if *is_export {
            prefix.push_str("export ");
        }
        if *is_inline {
            prefix.push_str("inline ");
        }
        prefix.push_str("fn ");
        prefix.push_str(name);

        // The signature tail after the parameter list: align, return type, and
        // either ` {` (body) or `;` (prototype).
        let mut tail = String::new();
        if let Some(a) = align {
            tail.push_str(" align(");
            tail.push_str(&self.flat_expr(a, 0));
            tail.push(')');
        }
        tail.push(' ');
        tail.push_str(&self.flat_expr(ret, 0));
        let open = if body.is_some() { " {" } else { ";" };

        let params_flat = self.flat_params(params, *is_varargs);
        let flat_sig = format!("{prefix}({params_flat}){tail}{open}");
        // A comment interior to the parameter list (before the return type, which
        // starts the signature tail) forces the broken-params form so each such
        // comment has a home next to its parameter instead of leaking into the
        // body / past the `;`.
        let has_param_comment = !params.is_empty() && self.has_comment_before(ret.span().start);
        let fits =
            self.depth * INDENT.len() + flat_sig.chars().count() <= WIDTH && !has_param_comment;

        match body {
            Some(stmts) => {
                if fits {
                    let header = format!("{prefix}({params_flat}){tail} {{");
                    self.print_stmt_block(&header, stmts, span.end);
                } else {
                    self.print_params_broken(
                        &prefix,
                        params,
                        *is_varargs,
                        &format!("{tail} {{"),
                        ret.span().start,
                    );
                    self.print_stmt_block_body(stmts, span.end);
                }
            }
            None => {
                if fits {
                    self.line(&flat_sig);
                } else {
                    self.print_params_broken(
                        &prefix,
                        params,
                        *is_varargs,
                        &format!("{tail};"),
                        ret.span().start,
                    );
                }
                self.attach_trailing(span.end, span.line);
            }
        }
    }

    /// Prints a broken parameter list: `prefix(` then one param per line at
    /// indent+1, then `)tail` de-indented. `tail` already includes any return
    /// type and the ` {`/`;`. `interior_end` is the offset where the parameter
    /// list ends (the return type's start), used to claim each parameter's
    /// own-line leading and same-line trailing comments so they stay attached to
    /// their parameter instead of leaking into the function body.
    fn print_params_broken(
        &mut self,
        prefix: &str,
        params: &[Param],
        varargs: bool,
        tail: &str,
        interior_end: u32,
    ) {
        self.line(&format!("{prefix}("));
        self.depth += 1;
        for (i, p) in params.iter().enumerate() {
            self.emit_leading_comments(p.span.start);
            self.blank_after_leading(self.line_at(p.span.start));
            let s = self.flat_param(p);
            self.line(&format!("{s},"));
            let next = params
                .get(i + 1)
                .map(|np| np.span.start)
                .unwrap_or(interior_end);
            self.attach_trailing_bounded(p.span.end, self.line_at(p.span.end), next);
        }
        if varargs {
            // `...` is terminal and never takes a trailing comma.
            self.line("...");
        }
        // Own-line comments after the last parameter but before the `)`.
        self.emit_dangling_comments(interior_end);
        self.depth -= 1;
        self.line(&format!("){tail}"));
    }

    // ---- blocks -----------------------------------------------------------

    /// Prints a `header` line that opens a brace block, then the statements, then
    /// the closing `}`. Empty bodies (and no interior comments) render as `{}`.
    fn print_stmt_block(&mut self, header: &str, stmts: &[Stmt], end_offset: u32) {
        // `header` ends in ` {`; an empty body collapses to `{}`.
        if stmts.is_empty() && !self.has_comment_before(end_offset) {
            let base = header.strip_suffix('{').unwrap_or(header).trim_end();
            self.line(&format!("{base} {{}}"));
            return;
        }
        self.line(header);
        // A same-line trailing comment on the opening `{` (e.g. `... void { // x`)
        // stays on the opener line rather than leaking to the bottom of the block.
        let content = stmts.first().map(|s| s.span().start).unwrap_or(end_offset);
        self.attach_block_opener_comment(content);
        self.print_stmt_block_body(stmts, end_offset);
    }

    /// Attaches a same-line trailing comment that sits on the block-opening `{`
    /// line. `content_start` is the offset of the first thing inside the block
    /// (the first statement, or the closing `}` for an empty body); the opener
    /// `{` is the nearest `{` strictly before it. A pending non-own-line comment
    /// whose line matches that brace's line is the opener's trailing comment.
    fn attach_block_opener_comment(&mut self, content_start: u32) {
        if let Some(brace) = self.brace_before(content_start) {
            self.attach_trailing_bounded(brace + 1, self.line_at(brace), content_start);
        }
    }

    /// The offset of the last `{` strictly before `before`, if any.
    fn brace_before(&self, before: u32) -> Option<u32> {
        let end = (before as usize).min(self.src.len());
        self.src[..end]
            .iter()
            .rposition(|&c| c == '{')
            .map(|i| i as u32)
    }

    /// Prints the body of a brace block (statements at indent+1) and the closing
    /// `}` at the current indent. Assumes the opening `{` was already emitted.
    fn print_stmt_block_body(&mut self, stmts: &[Stmt], end_offset: u32) {
        self.depth += 1;
        self.print_stmts(stmts);
        // Dangling comments before the closing brace.
        self.emit_dangling_comments(end_offset);
        self.depth -= 1;
        self.line("}");
    }

    /// Prints a block-bodied construct (`test`/`comptime`) whose `header` ends in
    /// ` {`.
    fn print_block_construct(&mut self, header: &str, stmts: &[Stmt], end_offset: u32) {
        self.print_stmt_block(header, stmts, end_offset);
    }

    /// Prints a sequence of statements, preserving single author blank lines
    /// between them (never adjacent to a brace) and interleaving comments.
    fn print_stmts(&mut self, stmts: &[Stmt]) {
        let mut prev_end_line: Option<u32> = None;
        for stmt in stmts {
            let start = stmt.span().start;
            let start_line = self.line_at(start);
            // A blank line the author left before this statement is preserved if
            // there is a real gap (more than one line) and no comment will fill
            // it (a leading comment carries its own blank-above flag).
            if let Some(pe) = prev_end_line {
                if start_line > pe + 1 && !self.has_comment_before(start) {
                    self.blank();
                }
            }
            self.emit_leading_comments(start);
            self.blank_after_leading(start_line);
            self.print_stmt(stmt);
            prev_end_line = Some(self.line_at(stmt.span().end));
        }
    }

    /// The source line on which `offset` sits (1-based), used to detect author
    /// blank lines between consecutive statements/members.
    fn line_at(&self, offset: u32) -> u32 {
        let n = (offset as usize).min(self.src.len());
        // Count newlines strictly before `offset`.
        1 + self.src[..n].iter().filter(|&&c| c == '\n').count() as u32
    }

    // ---- statements -------------------------------------------------------

    /// Prints one statement (no leading/trailing comment handling beyond the
    /// trailing same-line comment, which is attached here).
    fn print_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Const {
                name,
                ty,
                value,
                span,
            } => {
                let mut head = String::new();
                if self.comptime_precedes(span.start) {
                    head.push_str("comptime ");
                }
                head.push_str(&format!("const {name}"));
                if let Some(t) = ty {
                    head.push_str(": ");
                    head.push_str(&self.flat_expr(t, 0));
                }
                head.push_str(" = ");
                self.print_assigned_value(&head, value, ";", span.end);
            }
            Stmt::Var {
                name,
                ty,
                value,
                span,
            } => {
                let mut head = String::new();
                if self.comptime_precedes(span.start) {
                    head.push_str("comptime ");
                }
                head.push_str(&format!("var {name}"));
                if let Some(t) = ty {
                    head.push_str(": ");
                    head.push_str(&self.flat_expr(t, 0));
                }
                match value {
                    Some(v) => {
                        head.push_str(" = ");
                        self.print_assigned_value(&head, v, ";", span.end);
                    }
                    None => {
                        head.push(';');
                        self.line(&head);
                        self.attach_trailing(span.end, span.line);
                    }
                }
            }
            Stmt::Defer { body, span } => {
                self.print_defer_like("defer", None, body, span.end);
            }
            Stmt::Errdefer {
                capture,
                body,
                span,
            } => {
                self.print_defer_like("errdefer", capture.as_deref(), body, span.end);
            }
            Stmt::Return { value, span } => match value {
                Some(v) => {
                    self.print_assigned_value("return ", v, ";", span.end);
                }
                None => {
                    self.line("return;");
                    self.attach_trailing(span.end, span.line);
                }
            },
            Stmt::Expr { expr, span } => {
                // A bare/labeled block used as an expression statement
                // (`blk: { ... }`) takes NO trailing `;` — one would be a parse
                // error. Every other expression statement is `;`-terminated.
                let tail = if matches!(expr, Expr::Block { .. }) {
                    ""
                } else {
                    ";"
                };
                self.print_assigned_value("", expr, tail, span.end);
            }
            Stmt::Assign {
                target,
                op,
                value,
                span,
            } => {
                let head = format!("{} {} ", self.flat_expr(target, 0), assign_op_str(*op));
                self.print_assigned_value(&head, value, ";", span.end);
            }
            Stmt::Comptime { body, span } => {
                self.print_stmt_block("comptime {", body, span.end);
            }
            Stmt::Block { body, span } => {
                self.print_stmt_block("{", body, span.end);
            }
            Stmt::If { expr, span }
            | Stmt::While { expr, span }
            | Stmt::For { expr, span }
            | Stmt::Switch { expr, span } => {
                self.print_cf_stmt(expr, span.end);
            }
            Stmt::Break { label, value, span } => {
                let mut s = String::from("break");
                if let Some(l) = label {
                    s.push_str(&format!(" :{l}"));
                }
                match value {
                    Some(v) => {
                        s.push(' ');
                        let head = s;
                        self.print_assigned_value(&head, v, ";", span.end);
                    }
                    None => {
                        s.push(';');
                        self.line(&s);
                        self.attach_trailing(span.end, span.line);
                    }
                }
            }
            Stmt::Continue { label, span } => {
                let mut s = String::from("continue");
                if let Some(l) = label {
                    s.push_str(&format!(" :{l}"));
                }
                s.push(';');
                self.line(&s);
                self.attach_trailing(span.end, span.line);
            }
        }
    }

    /// Prints a `defer`/`errdefer` statement. A block body stays a block; an
    /// expression body is rendered inline and terminated by `;`.
    fn print_defer_like(&mut self, kw: &str, capture: Option<&str>, body: &Stmt, end_offset: u32) {
        let mut head = String::from(kw);
        if let Some(c) = capture {
            head.push_str(&format!(" |{c}|"));
        }
        head.push(' ');
        match body {
            Stmt::Block {
                body: inner, span, ..
            } => {
                let header = format!("{}{{", head);
                self.print_stmt_block(&header, inner, span.end);
            }
            Stmt::Expr { expr, span } => {
                self.print_assigned_value(&head, expr, ";", span.end);
            }
            other => {
                // The parser only ever produces a block or an expression body
                // for `defer`/`errdefer` (see `parse_defer_body`); this arm is a
                // defensive fallback that keeps output well-formed by emitting the
                // keyword line, then the inner statement on its own line.
                self.line(head.trim_end());
                self.print_stmt(other);
                let _ = end_offset;
            }
        }
    }

    // ---- control flow -----------------------------------------------------

    /// Prints a statement-form control-flow construct (no trailing `;`).
    fn print_cf_stmt(&mut self, expr: &Expr, end_offset: u32) {
        self.print_control_flow(expr, "", end_offset);
    }

    /// Prints a control-flow construct used as a value, with a leading `head`
    /// (e.g. `const x = `) and a trailing `tail` (e.g. `;`).
    fn print_cf_value(&mut self, head: &str, expr: &Expr, tail: &str, end_offset: u32) {
        self.print_control_flow_inner(head, expr, tail, end_offset);
    }

    /// Statement-form entry: no head, optional tail.
    fn print_control_flow(&mut self, expr: &Expr, tail: &str, end_offset: u32) {
        self.print_control_flow_inner("", expr, tail, end_offset);
    }

    /// The shared control-flow printer. `head` is emitted before the keyword and
    /// `tail` after the construct closes (used in value position).
    fn print_control_flow_inner(&mut self, head: &str, expr: &Expr, tail: &str, end_offset: u32) {
        match expr {
            Expr::If {
                cond,
                capture,
                then_branch,
                else_capture,
                else_branch,
                ..
            } => {
                let mut opener = format!("{head}if ({})", self.flat_expr(cond, 0));
                if let Some(cap) = capture {
                    opener.push(' ');
                    opener.push_str(&capture_str(cap));
                }
                let term =
                    self.print_branch(&opener, then_branch, else_branch.is_some(), end_offset);
                if let Some(eb) = else_branch {
                    let mut eopen = String::from("else");
                    if let Some(cap) = else_capture {
                        eopen.push(' ');
                        eopen.push_str(&capture_str(cap));
                    }
                    self.print_else_branch(&eopen, eb, tail, end_offset, term);
                } else if !tail.is_empty() {
                    self.append_tail(tail, end_offset);
                }
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
                let mut opener = String::new();
                opener.push_str(head);
                if let Some(l) = label {
                    opener.push_str(&format!("{l}: "));
                }
                if *is_inline {
                    opener.push_str("inline ");
                }
                opener.push_str(&format!("while ({})", self.flat_expr(cond, 0)));
                if let Some(cap) = capture {
                    opener.push(' ');
                    opener.push_str(&capture_str(cap));
                }
                if let Some(c) = cont {
                    opener.push_str(&format!(" : ({})", self.flat_stmt_inline(c)));
                }
                let term = self.print_branch(&opener, body, else_branch.is_some(), end_offset);
                if let Some(eb) = else_branch {
                    // `while ... else |e| ...` keeps its else-payload capture.
                    let mut eopen = String::from("else");
                    if let Some(cap) = else_capture {
                        eopen.push(' ');
                        eopen.push_str(&capture_str(cap));
                    }
                    self.print_else_branch(&eopen, eb, tail, end_offset, term);
                } else if !tail.is_empty() {
                    self.append_tail(tail, end_offset);
                }
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
                let mut opener = String::new();
                opener.push_str(head);
                if let Some(l) = label {
                    opener.push_str(&format!("{l}: "));
                }
                if *is_inline {
                    opener.push_str("inline ");
                }
                let ops = operands
                    .iter()
                    .map(|o| self.flat_for_operand(o))
                    .collect::<Vec<_>>()
                    .join(", ");
                let caps = captures
                    .iter()
                    .map(capture_name_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                opener.push_str(&format!("for ({ops}) |{caps}|"));
                let term = self.print_branch(&opener, body, else_branch.is_some(), end_offset);
                if let Some(eb) = else_branch {
                    self.print_else_branch("else", eb, tail, end_offset, term);
                } else if !tail.is_empty() {
                    self.append_tail(tail, end_offset);
                }
            }
            Expr::Switch {
                scrutinee, arms, ..
            } => {
                let opener = format!("{head}switch ({}) {{", self.flat_expr(scrutinee, 0));
                self.print_switch(&opener, arms, tail, end_offset);
            }
            Expr::Block { label, body, .. } => {
                let mut opener = String::from(head);
                if let Some(l) = label {
                    opener.push_str(&format!("{l}: "));
                }
                opener.push('{');
                self.print_stmt_block(&opener, body, end_offset);
                if !tail.is_empty() {
                    self.append_tail(tail, end_offset);
                }
            }
            _ => {
                // Not actually control flow: print as an assigned value.
                self.print_assigned_value(head, expr, tail, end_offset);
            }
        }
    }

    /// Prints the `then`/loop body branch after an `opener` line and reports how
    /// it terminated (see [`BranchTerm`]) so a following `else` can attach legally.
    ///
    /// A braced body becomes a multi-line block (`Braced`). A brace-less single
    /// *statement* body stays brace-less on one line when it fits (`Braceless`).
    /// A bare *expression* branch (only reachable in value position, e.g.
    /// `if (c) THEN else ELSE`) is emitted brace-less: inline after the opener if
    /// it fits, otherwise wrapped onto its own indented line — but never braced
    /// (bracing a bare expression would either change the AST to a `block` node or
    /// be unparseable without a `;`) and never terminated with `;` here (the `;`
    /// is the construct's `tail`, appended after the whole if-else).
    fn print_branch(
        &mut self,
        opener: &str,
        body: &Expr,
        has_else: bool,
        end_offset: u32,
    ) -> BranchTerm {
        match body {
            Expr::Block {
                label,
                body: stmts,
                span,
            } => {
                // The parser wraps BOTH a braced body and a brace-less single
                // statement in `Expr::Block`. Disambiguate by peeking at the
                // source: only keep it brace-less if the original had no `{` AND
                // it is a single non-control-flow statement that fits.
                let braced = label.is_some() || self.opened_with_brace(span.start);
                if !braced {
                    if let [only] = stmts.as_slice() {
                        // A comment between the opener and the brace-less body
                        // (`if (a) // c\n    stmt`) is kept on the opener line and
                        // the body wraps onto the next line, preserving placement
                        // and idempotence; otherwise the body stays on the opener
                        // line when it fits.
                        if self.has_comment_before(only.span().start) {
                            self.line(opener);
                            self.attach_opener_trailing_comment(only.span().start);
                            self.depth += 1;
                            self.emit_leading_comments(only.span().start);
                            self.last_comment_line = None;
                            self.print_stmt(only);
                            self.depth -= 1;
                            return BranchTerm::Braceless;
                        }
                        if self.try_braceless_branch(opener, only) {
                            return BranchTerm::Braceless;
                        }
                    }
                }
                // A labeled block keeps its `{label}: ` prefix on the brace
                // opener; dropping it would leave any interior `break :label`
                // dangling and change the AST (the block's label node vanishes).
                let lbl = label.as_ref().map(|l| format!("{l}: ")).unwrap_or_default();
                if stmts.is_empty() && !self.has_comment_before(span.end) {
                    self.line(&format!("{opener} {lbl}{{}}"));
                    // A same-line trailing comment on an empty `{}` branch
                    // (`if (x) {} // note`) stays on that line instead of being
                    // swallowed into a following `else` block.
                    self.attach_trailing(span.end, self.line_at(span.end));
                } else {
                    self.line(&format!("{opener} {lbl}{{"));
                    let content = stmts.first().map(|s| s.span().start).unwrap_or(span.end);
                    self.attach_block_opener_comment(content);
                    self.print_stmt_block_body(stmts, span.end);
                }
                BranchTerm::Braced
            }
            _ => {
                // A bare expression branch (value-position if/while/for). Emit it
                // brace-less. When no `else` follows and it fits, this is the
                // statement form's `if (c) expr;`; otherwise it is a value branch
                // whose terminating `;` belongs to the construct tail, not here.
                let flat = self.flat_expr(body, 0);
                let semi = if has_else { "" } else { ";" };
                // A comment sitting between the opener and the bare branch (e.g.
                // `if (a) // note\n    b`) forces the wrapped form so the comment
                // keeps its home on the opener line; emitting it flat would dump
                // the comment past the statement and break idempotence.
                let comment_in_branch = self.has_comment_before(body.span().start);
                let candidate = format!("{opener} {flat}{semi}");
                if self.depth * INDENT.len() + candidate.chars().count() <= WIDTH
                    && !flat.contains('\n')
                    && !comment_in_branch
                {
                    self.line(&candidate);
                } else {
                    // Wrap the brace-less branch onto its own indented line. We do
                    // NOT brace it (that would alter the AST) and do NOT append the
                    // construct tail (the caller appends it after the else).
                    self.line(opener);
                    // A same-line trailing comment on the opener (`if (a) // c`)
                    // stays on the opener line.
                    self.attach_opener_trailing_comment(body.span().start);
                    self.depth += 1;
                    // Own-line comments between the opener and the branch body.
                    self.emit_leading_comments(body.span().start);
                    self.last_comment_line = None;
                    self.print_assigned_value("", body, semi, end_offset);
                    self.depth -= 1;
                }
                BranchTerm::Braceless
            }
        }
    }

    /// Tries to print a brace-less single-statement control-flow body on the
    /// `opener` line. Returns `true` if it did. Only simple statements (a bare
    /// expression, `return`, `break`, `continue`, assignment) that fit in the
    /// print width qualify; anything else falls back to a brace block.
    fn try_braceless_branch(&mut self, opener: &str, stmt: &Stmt) -> bool {
        let rendered = match stmt {
            Stmt::Expr { expr, .. } => Some(format!("{};", self.flat_expr(expr, 0))),
            Stmt::Return { value: Some(v), .. } => {
                Some(format!("return {};", self.flat_expr(v, 0)))
            }
            Stmt::Return { value: None, .. } => Some("return;".to_string()),
            Stmt::Break { label, value, .. } => {
                let mut s = String::from("break");
                if let Some(l) = label {
                    s.push_str(&format!(" :{l}"));
                }
                if let Some(v) = value {
                    s.push_str(&format!(" {}", self.flat_expr(v, 0)));
                }
                s.push(';');
                Some(s)
            }
            Stmt::Continue { label, .. } => {
                let mut s = String::from("continue");
                if let Some(l) = label {
                    s.push_str(&format!(" :{l}"));
                }
                s.push(';');
                Some(s)
            }
            Stmt::Assign {
                target, op, value, ..
            } => Some(format!(
                "{} {} {};",
                self.flat_expr(target, 0),
                assign_op_str(*op),
                self.flat_expr(value, 0)
            )),
            _ => None,
        };
        match rendered {
            Some(body) if !body.contains('\n') => {
                let candidate = format!("{opener} {body}");
                if self.depth * INDENT.len() + candidate.chars().count() <= WIDTH {
                    self.line(&candidate);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Prints an `else`/`else if`/`else |e|` branch. When the preceding then-body
    /// was braced (`then_term == Braced`) the `else` joins its closing `}` as
    /// `} else …`; when it was brace-less (`then_term == Braceless`) the `else`
    /// begins on its own fresh line — joining a non-`}` line or terminating the
    /// then-body with `;` would emit unparseable orphaned-`else` output.
    fn print_else_branch(
        &mut self,
        eopen: &str,
        body: &Expr,
        tail: &str,
        end_offset: u32,
        then_term: BranchTerm,
    ) {
        // An own-line comment the author placed between the then-branch `}` and
        // the `else` keyword documents the else; emit it on its own line first,
        // which also forces the `else` onto a fresh line (the canonical, and
        // re-parseable, `}` / `// comment` / `else {` shape) instead of letting
        // it leak inside the else block.
        let comment_before_else = self.has_comment_before(body.span().start)
            && self.comments.get(self.cursor).is_some_and(|c| c.own_line);
        if comment_before_else && then_term == BranchTerm::Braced {
            // Close the then-`}` first (it is already on its own line), then the
            // comment, then `else` fresh.
            self.emit_leading_comments(body.span().start);
            // The comment leads the `else`, not the first else-body statement, so
            // forget its line: otherwise `blank_after_leading` would inject a
            // spurious blank above the first statement inside the else block.
            self.last_comment_line = None;
            self.line(eopen);
        } else {
            match then_term {
                BranchTerm::Braced => self.join_to_closing_brace(eopen),
                // No `}` to join: start the `else` on its own line at this indent.
                BranchTerm::Braceless => self.line(eopen),
            }
        }
        match body {
            // `else if (...)` chains: print the nested if on the same line.
            Expr::If { .. } | Expr::While { .. } | Expr::For { .. } | Expr::Switch { .. } => {
                // Re-emit the joined `} else ` then the control flow inline.
                // We already wrote `} else`; append a space and the construct.
                self.append_to_last(" ");
                self.print_control_flow_inner("", body, tail, end_offset);
            }
            Expr::Block {
                label,
                body: stmts,
                span,
            } => {
                // Preserve a labeled else-block's `{label}: ` prefix on the joined
                // `} else {label}: {` line (the label is part of the AST and may
                // be referenced by an interior `break :label`).
                let lbl = label.as_ref().map(|l| format!("{l}: ")).unwrap_or_default();
                if stmts.is_empty() && !self.has_comment_before(span.end) {
                    self.append_to_last(&format!(" {lbl}{{}}"));
                    if !tail.is_empty() {
                        self.append_tail(tail, end_offset);
                    }
                } else {
                    self.append_to_last(&format!(" {lbl}{{"));
                    let content = stmts.first().map(|s| s.span().start).unwrap_or(span.end);
                    self.attach_block_opener_comment(content);
                    self.print_stmt_block_body(stmts, span.end);
                    if !tail.is_empty() {
                        self.append_tail(tail, end_offset);
                    }
                }
            }
            _ => {
                // Brace-less else body. Render it inline after `else` if it fits,
                // otherwise wrap it onto its own indented line. The construct tail
                // (`;`) follows the body.
                let flat = self.flat_expr(body, 0);
                let inline = format!(" {flat}{tail}");
                let col = self.last_line_width();
                if col + inline.chars().count() <= WIDTH && !flat.contains('\n') {
                    self.append_to_last(&inline);
                } else {
                    self.depth += 1;
                    self.print_assigned_value("", body, tail, end_offset);
                    self.depth -= 1;
                }
            }
        }
    }

    /// The display width (in scalars) of the last emitted line, used to decide
    /// whether more text can be appended within the print width.
    fn last_line_width(&self) -> usize {
        let trimmed = self.out.strip_suffix('\n').unwrap_or(&self.out);
        let last = trimmed.rsplit('\n').next().unwrap_or("");
        last.chars().count()
    }

    /// Turns the last emitted line (a lone `}`) into `} <text>` so an `else`
    /// joins the closing brace. If the last line is not a lone `}`, appends a new
    /// line instead (defensive).
    fn join_to_closing_brace(&mut self, text: &str) {
        // The output ends with "...}\n"; strip the trailing newline and append.
        if self.out.ends_with("}\n") {
            self.out.pop(); // remove '\n'
            self.out.push(' ');
            self.out.push_str(text);
            self.out.push('\n');
        } else {
            self.line(text);
        }
    }

    /// Appends raw text to the last emitted line (before its newline).
    fn append_to_last(&mut self, text: &str) {
        if self.out.ends_with('\n') {
            self.out.pop();
            self.out.push_str(text);
            self.out.push('\n');
        } else {
            self.out.push_str(text);
        }
    }

    /// Appends a trailing `tail` (e.g. `;`) to the last emitted line.
    fn append_tail(&mut self, tail: &str, end_offset: u32) {
        self.append_to_last(tail);
        let _ = end_offset;
    }

    /// Prints a `switch` body: opener line, one arm per line at indent+1, `}`.
    fn print_switch(&mut self, opener: &str, arms: &[SwitchArm], tail: &str, end_offset: u32) {
        if arms.is_empty() && !self.has_comment_before(end_offset) {
            let base = opener.strip_suffix('{').unwrap_or(opener).trim_end();
            self.line(&format!("{base} {{}}"));
            if !tail.is_empty() {
                self.append_tail(tail, end_offset);
            }
            return;
        }
        self.line(opener);
        let content = arms.first().map(|a| a.span.start).unwrap_or(end_offset);
        self.attach_block_opener_comment(content);
        self.depth += 1;
        for arm in arms {
            self.emit_leading_comments(arm.span.start);
            self.blank_after_leading(arm.span.line);
            self.print_switch_arm(arm);
        }
        self.emit_dangling_comments(end_offset);
        self.depth -= 1;
        self.line("}");
        if !tail.is_empty() {
            self.append_tail(tail, end_offset);
        }
    }

    /// Prints one `switch` arm, with a trailing comma after every arm.
    fn print_switch_arm(&mut self, arm: &SwitchArm) {
        let pat = self.flat_switch_pattern(&arm.pattern);
        let mut opener = format!("{pat} =>");
        if let Some(cap) = &arm.capture {
            opener.push(' ');
            opener.push_str(&capture_str(cap));
        }
        match &arm.body {
            Expr::Block {
                label,
                body: stmts,
                span,
            } => {
                // Keep a labeled arm-block's `{label}: ` prefix (AST-significant).
                let lbl = label.as_ref().map(|l| format!("{l}: ")).unwrap_or_default();
                if stmts.is_empty() && !self.has_comment_before(span.end) {
                    self.line(&format!("{opener} {lbl}{{}},"));
                } else {
                    self.line(&format!("{opener} {lbl}{{"));
                    let content = stmts.first().map(|s| s.span().start).unwrap_or(span.end);
                    self.attach_block_opener_comment(content);
                    self.depth += 1;
                    self.print_stmts(stmts);
                    self.emit_dangling_comments(span.end);
                    self.depth -= 1;
                    self.line("},");
                }
            }
            other => {
                let head = format!("{opener} ");
                self.print_assigned_value(&head, other, ",", arm.span.end);
            }
        }
    }

    // ---- containers -------------------------------------------------------

    /// Prints a container value (`struct`/`enum`/`union`) opened on `head`, with
    /// members at indent+1, closed by `}` plus `tail` (e.g. `;`).
    fn print_container(&mut self, head: &str, c: &Container, tail: &str, end_offset: u32) {
        let kw = container_keyword(&c.kind, self);

        // A single-line struct of only-fields that fits stays flat:
        // `const Color = struct { r: u8, g: u8, b: u8 };`.
        if let Some(flat) = self.try_flat_container(head, c, tail) {
            self.line(&flat);
            self.attach_trailing(end_offset, self.line_at(end_offset));
            return;
        }

        self.line(&format!("{head}{kw} {{"));
        let content = c
            .members
            .first()
            .map(|m| m.span().start)
            .unwrap_or(end_offset);
        self.attach_block_opener_comment(content);
        self.depth += 1;
        self.print_members(&c.members, end_offset);
        self.depth -= 1;
        self.line(&format!("}}{tail}"));
        // The closing `}` sits on the container's END line, not its start line;
        // match the trailing comment against the end line so `}; // note` keeps
        // its comment on the brace line.
        self.attach_trailing(end_offset, self.line_at(end_offset));
    }

    /// Attempts a single-line container rendering, allowed only when the body is
    /// non-empty, all members are fields with no doc comments, there are no
    /// interior comments, and the whole thing fits in the print width.
    fn try_flat_container(&mut self, head: &str, c: &Container, tail: &str) -> Option<String> {
        if self.has_comment_before(c.span.end) {
            return None;
        }
        if c.members.is_empty() {
            return None;
        }
        // Only collapse a container the author already wrote on a single source
        // line; a multi-line container stays multi-line. (The AST does not record
        // this, so we read it back from the original span.)
        if self.line_at(c.span.start) != self.line_at(c.span.end) {
            return None;
        }
        let mut fields = Vec::new();
        for m in &c.members {
            match m {
                Member::Field(f) if f.doc.is_none() => fields.push(self.flat_field(f)),
                _ => return None,
            }
        }
        let kw = container_keyword(&c.kind, self);
        let body = fields.join(", ");
        let s = format!("{head}{kw} {{ {body} }}{tail}");
        if self.depth * INDENT.len() + s.chars().count() <= WIDTH {
            Some(s)
        } else {
            None
        }
    }

    /// Prints container members (fields end with `,`; nested decls print as
    /// items), preserving single author blank lines between groups.
    fn print_members(&mut self, members: &[Member], end_offset: u32) {
        let mut prev_line: Option<u32> = None;
        for m in members {
            let start = m.span().start;
            let start_line = self.line_at(start);
            if let Some(pl) = prev_line {
                if start_line > pl + 1 && !self.has_comment_before(start) {
                    self.blank();
                }
            }
            self.emit_leading_comments(start);
            self.blank_after_leading(start_line);
            match m {
                Member::Field(f) => {
                    // A field's own doc comment.
                    if let Some(doc) = &f.doc {
                        for line in doc_lines(doc) {
                            self.line(&line);
                        }
                    }
                    let s = self.flat_field(f);
                    self.line(&format!("{s},"));
                    self.attach_trailing(f.span.end, f.span.line);
                }
                Member::Decl(item) => {
                    self.print_item(item);
                }
            }
            prev_line = Some(self.line_at(m.span().end));
        }
        self.emit_dangling_comments(end_offset);
    }

    // ---- initializers & calls (broken forms) ------------------------------

    /// Prints a broken `.{ ... }` / `T{ ... }` initializer: opener, one element
    /// per line at indent+1, `}` plus tail.
    fn print_init_broken(
        &mut self,
        head: &str,
        ty: Option<&Expr>,
        body: &InitBody,
        tail: &str,
        end_offset: u32,
    ) {
        let prefix = match ty {
            Some(t) => self.flat_expr(t, 0),
            None => ".".to_string(),
        };
        self.line(&format!("{head}{prefix}{{"));
        self.depth += 1;
        match body {
            InitBody::Fields(fields) => {
                for (i, f) in fields.iter().enumerate() {
                    self.emit_leading_comments(f.span.start);
                    self.blank_after_leading(self.line_at(f.span.start));
                    self.print_list_element(&format!(".{} = ", f.name), &f.value);
                    let next = fields
                        .get(i + 1)
                        .map(|n| n.span.start)
                        .unwrap_or(end_offset);
                    self.attach_trailing_bounded(f.span.end, self.line_at(f.span.end), next);
                }
            }
            InitBody::Tuple(elems) => {
                for (i, e) in elems.iter().enumerate() {
                    let start = e.span().start;
                    self.emit_leading_comments(start);
                    self.blank_after_leading(self.line_at(start));
                    self.print_list_element("", e);
                    let next = elems
                        .get(i + 1)
                        .map(|n| n.span().start)
                        .unwrap_or(end_offset);
                    self.attach_trailing_bounded(e.span().end, self.line_at(e.span().end), next);
                }
            }
        }
        self.emit_dangling_comments(end_offset);
        self.depth -= 1;
        self.line(&format!("}}{tail}"));
        self.attach_trailing(end_offset, self.line_at(end_offset));
    }

    /// Prints a broken call/builtin: `head` + `callee(` then one arg per line.
    fn print_call_broken(
        &mut self,
        head: &str,
        callee: &str,
        args: &[Expr],
        tail: &str,
        end_offset: u32,
    ) {
        // A single argument that is itself a breakable init/call: break that
        // child rather than the arg list, matching the `b.addExecutable(.{...})`
        // idiom.
        if args.len() == 1 {
            if let Expr::Init { ty, body, .. } = &args[0] {
                self.print_init_broken(
                    &format!("{head}{callee}("),
                    ty.as_deref(),
                    body,
                    &format!("){tail}"),
                    end_offset,
                );
                return;
            }
            // A single binary chain too long to fit: keep the `(` on the opener
            // line and greedily wrap the chain, continuation indented one level.
            if is_flat_binary_chain(&args[0]) {
                let opener = format!("{head}{callee}(");
                self.print_binary_wrapped(&opener, &args[0], &format!("){tail}"));
                self.attach_trailing(end_offset, 0);
                return;
            }
        }
        self.line(&format!("{head}{callee}("));
        self.depth += 1;
        for (i, a) in args.iter().enumerate() {
            // Each argument owns its own leading own-line comments and its
            // same-line trailing comment, mirroring the init/field paths so an
            // interior `// c` stays attached to its arg (preserving idempotence
            // and correct placement) instead of falling through to a dangling
            // comment below the whole call.
            self.emit_leading_comments(a.span().start);
            self.blank_after_leading(self.line_at(a.span().start));
            self.print_list_element("", a);
            let next = args
                .get(i + 1)
                .map(|n| n.span().start)
                .unwrap_or(end_offset);
            self.attach_trailing_bounded(a.span().end, self.line_at(a.span().end), next);
        }
        // Any comments authored just before the closing `)` (e.g. after the last
        // argument's comma, on their own line) stay inside the arg list.
        self.emit_dangling_comments(end_offset);
        self.depth -= 1;
        self.line(&format!("){tail}"));
        self.attach_trailing(end_offset, self.line_at(end_offset));
    }

    /// Greedily wraps a same-precedence binary chain after an `opener`, with the
    /// binary operator trailing each line and continuation operands indented one
    /// level past the opener's indent. `tail` (e.g. `);`) follows the last
    /// operand. This reproduces the hand-wrapped `a ++ b ++\n    c ++ d` idiom for
    /// long concatenations and similar chains.
    fn print_binary_wrapped(&mut self, opener: &str, chain: &Expr, tail: &str) {
        let (operands, ops) = flatten_binary_chain(chain);
        let rendered: Vec<String> = operands
            .iter()
            .map(|o| self.flat_child(o, chain, Side::Left))
            .collect();
        let base_col = self.depth * INDENT.len();
        let cont_col = base_col + INDENT.len();

        // Start the first line with the opener and the first operand.
        let mut line = format!("{opener}{}", rendered[0]);
        let mut on_first_line = true;
        for (i, op) in ops.iter().enumerate() {
            // The operator trails the current line.
            line.push_str(&format!(" {op}"));
            let next = &rendered[i + 1];
            let cur_col = if on_first_line { base_col } else { cont_col };
            // Does `next` still fit on the current line?
            let needed = cur_col + line.chars().count() + 1 + next.chars().count();
            let is_last = i + 1 == ops.len();
            let extra = if is_last { tail.chars().count() } else { 0 };
            if needed + extra <= WIDTH {
                line.push_str(&format!(" {next}"));
            } else {
                // Flush the current line and start a continuation.
                self.out.push_str(&INDENT.repeat(if on_first_line {
                    self.depth
                } else {
                    self.depth + 1
                }));
                self.out.push_str(&line);
                self.out.push('\n');
                on_first_line = false;
                line = next.clone();
            }
        }
        line.push_str(tail);
        let pad_levels = if on_first_line {
            self.depth
        } else {
            self.depth + 1
        };
        self.out.push_str(&INDENT.repeat(pad_levels));
        self.out.push_str(&line);
        self.out.push('\n');
    }

    /// Prints one element of a broken list (init field/tuple element, call
    /// argument) at the current indent: `prefix` + the value + a trailing comma.
    ///
    /// The element is emitted flat on one line when the whole `prefix value,`
    /// fits in the print width AND the value has no interior comment; otherwise
    /// the value is broken **in place on the live printer** (so any interior
    /// comment is emitted at its correct nesting level by the comment-aware
    /// `print_broken_value`, never relocated to a detached comment-blind buffer).
    /// Measuring the full `prefix value ,` width — not just the bare value —
    /// keeps `.name = value,` lines inside 100 columns.
    fn print_list_element(&mut self, prefix: &str, e: &Expr) {
        let col = self.depth * INDENT.len() + prefix.chars().count();
        let flat = self.flat_expr(e, col);
        // `< WIDTH` reserves the final column for the trailing comma the flat
        // branch appends (equivalent to `prefix value ,` fitting in WIDTH).
        let fits = col + flat.chars().count() < WIDTH && !flat.contains('\n');
        let has_interior_comment = self.has_comment_before(e.span().end);
        if fits && !has_interior_comment {
            self.line(&format!("{prefix}{flat},"));
        } else {
            // Break the value in place; `prefix` opens it and `,` closes it.
            self.print_broken_value(prefix, e, ",", e.span().end);
        }
    }

    // ---- attach trailing comment helper -----------------------------------

    /// Attaches a trailing same-line comment to the just-emitted line, if one
    /// exists for `end_offset`/`end_line`. The comment is separated from the code
    /// by a single space, matching the examples' house style.
    fn attach_trailing(&mut self, end_offset: u32, end_line: u32) {
        if let Some(text) = self.take_trailing_comment(end_offset, end_line) {
            self.append_to_last(&format!(" {text}"));
        }
    }

    /// Like [`Printer::attach_trailing`] but only claims a comment that starts
    /// before `next_bound`, so an element shared on a source line with a later
    /// sibling does not steal the later sibling's trailing comment.
    fn attach_trailing_bounded(&mut self, end_offset: u32, end_line: u32, next_bound: u32) {
        if let Some(text) = self.take_trailing_comment_bounded(end_offset, end_line, next_bound) {
            self.append_to_last(&format!(" {text}"));
        }
    }

    /// Appends a same-line trailing comment that sits on a just-emitted opener
    /// line (e.g. `if (a) // note`). Claims the pending comment when it is a
    /// non-own-line comment starting before `bound` (the branch body's start),
    /// regardless of the exact line — the opener line is synthesized so there is
    /// no stored source line to match against.
    fn attach_opener_trailing_comment(&mut self, bound: u32) {
        if let Some(c) = self.comments.get(self.cursor) {
            if !c.own_line && c.start < bound {
                let text = c.text.clone();
                self.cursor += 1;
                self.append_to_last(&format!(" {text}"));
            }
        }
    }

    // =====================================================================
    //  Flat (single-line) rendering
    // =====================================================================

    /// Renders an expression to a single line, obeying all spacing rules. `col`
    /// is the starting column (used only to keep recursion uniform; the flat
    /// renderer never itself breaks).
    fn flat_expr(&self, e: &Expr, col: usize) -> String {
        let _ = col;
        match e {
            Expr::Int { text, .. }
            | Expr::Float { text, .. }
            | Expr::Str { text, .. }
            | Expr::Char { text, .. } => text.clone(),
            Expr::Bool { value, .. } => {
                if *value {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            Expr::Null { .. } => "null".into(),
            Expr::Undefined { .. } => "undefined".into(),
            Expr::Unreachable { .. } => "unreachable".into(),
            Expr::AnyType { .. } => "anytype".into(),
            Expr::Ident { name, .. } => name.clone(),
            Expr::EnumLiteral { name, .. } => format!(".{name}"),
            Expr::ErrorLiteral { name, .. } => format!("error.{name}"),
            Expr::Builtin { name, args, .. } => {
                format!("{name}({})", self.flat_args(args))
            }
            Expr::Field { base, field, .. } => {
                format!("{}.{field}", self.flat_child(base, e, Side::Postfix))
            }
            Expr::Call { callee, args, .. } => {
                format!(
                    "{}({})",
                    self.flat_child(callee, e, Side::Postfix),
                    self.flat_args(args)
                )
            }
            Expr::Index { base, index, .. } => {
                format!(
                    "{}[{}]",
                    self.flat_child(base, e, Side::Postfix),
                    self.flat_expr(index, 0)
                )
            }
            Expr::SliceExpr { base, lo, hi, .. } => {
                let hi_s = hi
                    .as_ref()
                    .map(|h| self.flat_expr(h, 0))
                    .unwrap_or_default();
                format!(
                    "{}[{}..{}]",
                    self.flat_child(base, e, Side::Postfix),
                    self.flat_expr(lo, 0),
                    hi_s
                )
            }
            Expr::Deref { base, .. } => {
                format!("{}.*", self.flat_child(base, e, Side::Postfix))
            }
            Expr::Unwrap { base, .. } => {
                format!("{}.?", self.flat_child(base, e, Side::Postfix))
            }
            Expr::Binary { op, lhs, rhs, .. } => self.flat_binary(*op, lhs, rhs, e),
            Expr::Unary { op, operand, .. } => {
                let inner = self.flat_child(operand, e, Side::Right);
                match op {
                    UnOp::Neg => format!("-{inner}"),
                    UnOp::BitNot => format!("~{inner}"),
                    UnOp::AddrOf => format!("&{inner}"),
                    UnOp::Not => format!("not {inner}"),
                    UnOp::Try => format!("try {inner}"),
                }
            }
            Expr::Comptime { inner, .. } => {
                format!("comptime {}", self.flat_child(inner, e, Side::Right))
            }
            Expr::Catch {
                lhs, capture, rhs, ..
            } => {
                let cap = match capture {
                    Some(c) => format!("|{c}| "),
                    None => String::new(),
                };
                format!(
                    "{} catch {}{}",
                    self.flat_child(lhs, e, Side::Left),
                    cap,
                    self.flat_child(rhs, e, Side::Right)
                )
            }
            // ---- type constructors ----
            Expr::Optional { inner, .. } => format!("?{}", self.flat_child(inner, e, Side::Right)),
            Expr::Pointer {
                is_const,
                align,
                inner,
                ..
            } => {
                let mut s = String::from("*");
                if let Some(a) = align {
                    s.push_str(&format!("align({}) ", self.flat_expr(a, 0)));
                }
                if *is_const {
                    s.push_str("const ");
                }
                s.push_str(&self.flat_child(inner, e, Side::Right));
                s
            }
            Expr::Slice {
                is_const,
                align,
                inner,
                ..
            } => {
                let mut s = String::from("[]");
                if let Some(a) = align {
                    s.push_str(&format!("align({}) ", self.flat_expr(a, 0)));
                }
                if *is_const {
                    s.push_str("const ");
                }
                s.push_str(&self.flat_child(inner, e, Side::Right));
                s
            }
            Expr::ArrayType { len, inner, .. } => {
                format!(
                    "[{}]{}",
                    self.flat_expr(len, 0),
                    self.flat_child(inner, e, Side::Right)
                )
            }
            Expr::ErrorUnion { err, ok, .. } => {
                let ok_s = self.flat_child(ok, e, Side::Right);
                match err {
                    Some(er) => format!("{}!{ok_s}", self.flat_err_side(er)),
                    None => format!("!{ok_s}"),
                }
            }
            Expr::FnType {
                params,
                is_varargs,
                ret,
                ..
            } => {
                format!(
                    "fn({}) {}",
                    self.flat_params(params, *is_varargs),
                    self.flat_expr(ret, 0)
                )
            }
            Expr::ErrorSet { fields, .. } => {
                // Single field is tight (`error{X}`); multiple fields pad
                // (`error{ A, B }`); empty is `error{}`. Matches the examples.
                match fields.len() {
                    0 => "error{}".into(),
                    1 => format!("error{{{}}}", fields[0]),
                    _ => format!("error{{ {} }}", fields.join(", ")),
                }
            }
            Expr::Init { ty, body, .. } => self.flat_init(ty.as_deref(), body),
            Expr::Container(c) => self.flat_container_inline(c),
            Expr::Block { label, body, .. } => self.flat_block(label.as_deref(), body),
            Expr::If { .. } | Expr::While { .. } | Expr::For { .. } | Expr::Switch { .. } => {
                self.flat_control_flow(e)
            }
        }
    }

    /// Renders the error side of `E!T`, parenthesizing a `||` merge.
    fn flat_err_side(&self, err: &Expr) -> String {
        if matches!(
            err,
            Expr::Binary {
                op: BinOp::ErrSetMerge,
                ..
            }
        ) {
            format!("({})", self.flat_expr(err, 0))
        } else {
            self.flat_expr(err, 0)
        }
    }

    /// Renders a binary operation flat, inserting minimal parentheses.
    fn flat_binary(&self, op: BinOp, lhs: &Expr, rhs: &Expr, parent: &Expr) -> String {
        let opstr = bin_op_str(op);
        format!(
            "{} {opstr} {}",
            self.flat_child(lhs, parent, Side::Left),
            self.flat_child(rhs, parent, Side::Right)
        )
    }

    /// Renders a child expression flat, wrapping it in parentheses iff precedence
    /// or associativity requires it for the given `side` of `parent`.
    fn flat_child(&self, child: &Expr, parent: &Expr, side: Side) -> String {
        let s = self.flat_expr(child, 0);
        if needs_parens(parent, child, side) {
            format!("({s})")
        } else {
            s
        }
    }

    /// Renders a flat argument list (call/builtin args).
    fn flat_args(&self, args: &[Expr]) -> String {
        args.iter()
            .map(|a| self.flat_expr(a, 0))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Renders a flat parameter list (fn decl / fn type).
    fn flat_params(&self, params: &[Param], varargs: bool) -> String {
        let mut parts: Vec<String> = params.iter().map(|p| self.flat_param(p)).collect();
        if varargs {
            parts.push("...".to_string());
        }
        parts.join(", ")
    }

    /// Renders one parameter flat: `[comptime ]name: type` (or just the type for
    /// an unnamed fn-type parameter).
    fn flat_param(&self, p: &Param) -> String {
        let mut s = String::new();
        if p.is_comptime {
            s.push_str("comptime ");
        }
        if p.name.is_empty() {
            s.push_str(&self.flat_expr(&p.ty, 0));
        } else {
            s.push_str(&p.name);
            s.push_str(": ");
            s.push_str(&self.flat_expr(&p.ty, 0));
        }
        s
    }

    /// Renders a `for` operand flat.
    fn flat_for_operand(&self, o: &ForOperand) -> String {
        match o {
            ForOperand::Value(e) => self.flat_expr(e, 0),
            ForOperand::Range { lo, hi, .. } => {
                let hi_s = hi
                    .as_ref()
                    .map(|h| self.flat_expr(h, 0))
                    .unwrap_or_default();
                format!("{}..{}", self.flat_expr(lo, 0), hi_s)
            }
        }
    }

    /// Renders an initializer flat: `.{ ... }` / `T{ ... }` with one inner pad.
    fn flat_init(&self, ty: Option<&Expr>, body: &InitBody) -> String {
        // The init type prefix binds tighter than the `{...}` it introduces, so
        // it never needs parentheses; render it directly.
        let prefix = match ty {
            Some(t) => self.flat_expr(t, 0),
            None => ".".to_string(),
        };
        // Padding rule (matching the examples): named-field inits always pad
        // (`T{ .x = 1 }`); a tuple pads only when it has MORE THAN ONE element
        // (`.{ a, b }`), so a single-element tuple is tight (`.{x}`) and the
        // empty init is `.{}`/`T{}`.
        let (inner, pad) = match body {
            InitBody::Fields(fs) => {
                let s = fs
                    .iter()
                    .map(|f| format!(".{} = {}", f.name, self.flat_expr(&f.value, 0)))
                    .collect::<Vec<_>>()
                    .join(", ");
                (s, !fs.is_empty())
            }
            InitBody::Tuple(es) => {
                let s = es
                    .iter()
                    .map(|e| self.flat_expr(e, 0))
                    .collect::<Vec<_>>()
                    .join(", ");
                (s, es.len() > 1)
            }
        };
        if inner.is_empty() {
            format!("{prefix}{{}}")
        } else if pad {
            format!("{prefix}{{ {inner} }}")
        } else {
            format!("{prefix}{{{inner}}}")
        }
    }

    /// Renders a container inline (used when measuring; the real container path
    /// is `print_container`).
    fn flat_container_inline(&self, c: &Container) -> String {
        let kw = container_keyword(&c.kind, self);
        if c.members.is_empty() {
            return format!("{kw} {{}}");
        }
        let mut parts = Vec::new();
        for m in &c.members {
            match m {
                Member::Field(f) => parts.push(self.flat_field(f)),
                Member::Decl(_) => {
                    // A container with nested decls cannot be inlined; signal an
                    // over-long marker so callers always break it.
                    return format!("{kw} {{ /* ... */ }}");
                }
            }
        }
        format!("{kw} {{ {} }}", parts.join(", "))
    }

    /// Renders one field flat: `name: type[ align(e)][ = default]` (struct) or
    /// `name[ = value]` (enum).
    fn flat_field(&self, f: &Field) -> String {
        let mut s = String::new();
        if f.is_pub {
            s.push_str("pub ");
        }
        if f.is_comptime {
            s.push_str("comptime ");
        }
        s.push_str(&f.name);
        if let Some(ty) = &f.ty {
            s.push_str(": ");
            s.push_str(&self.flat_expr(ty, 0));
        }
        if let Some(a) = &f.align {
            s.push_str(&format!(" align({})", self.flat_expr(a, 0)));
        }
        if let Some(d) = &f.default {
            s.push_str(&format!(" = {}", self.flat_expr(d, 0)));
        }
        s
    }

    /// Renders a bare/labeled block flat (only valid when empty or single-line;
    /// non-empty blocks contain a newline marker so callers always break them).
    fn flat_block(&self, label: Option<&str>, body: &[Stmt]) -> String {
        let lbl = label.map(|l| format!("{l}: ")).unwrap_or_default();
        if body.is_empty() {
            format!("{lbl}{{}}")
        } else {
            // Force a break: a non-empty block is never single-line.
            format!("{lbl}{{\n}}")
        }
    }

    /// Renders control flow flat; non-trivial bodies force a break via a newline.
    ///
    /// The flat form must spell EVERY structural part — the payload captures
    /// (`|v|`, `else |e|`), loop labels and `inline`, and the `while` continue
    /// clause — because a fitting flat form is emitted verbatim; dropping any of
    /// them here would silently delete it from the output (an AST change).
    fn flat_control_flow(&self, e: &Expr) -> String {
        match e {
            Expr::Switch { .. } => "switch {\n}".into(),
            Expr::If {
                cond,
                capture,
                then_branch,
                else_capture,
                else_branch,
                ..
            } => {
                let cap = capture
                    .as_ref()
                    .map(|c| format!(" {}", capture_str(c)))
                    .unwrap_or_default();
                let t = self.flat_branch(then_branch);
                match else_branch {
                    Some(eb) => {
                        let ecap = else_capture
                            .as_ref()
                            .map(|c| format!("{} ", capture_str(c)))
                            .unwrap_or_default();
                        format!(
                            "if ({}){cap} {t} else {ecap}{}",
                            self.flat_expr(cond, 0),
                            self.flat_branch(eb)
                        )
                    }
                    None => format!("if ({}){cap} {t}", self.flat_expr(cond, 0)),
                }
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
                let mut s = String::new();
                if let Some(l) = label {
                    s.push_str(&format!("{l}: "));
                }
                if *is_inline {
                    s.push_str("inline ");
                }
                s.push_str(&format!("while ({})", self.flat_expr(cond, 0)));
                if let Some(c) = capture {
                    s.push_str(&format!(" {}", capture_str(c)));
                }
                if let Some(c) = cont {
                    s.push_str(&format!(" : ({})", self.flat_stmt_inline(c)));
                }
                s.push_str(&format!(" {}", self.flat_branch(body)));
                if let Some(eb) = else_branch {
                    let ecap = else_capture
                        .as_ref()
                        .map(|c| format!("{} ", capture_str(c)))
                        .unwrap_or_default();
                    s.push_str(&format!(" else {ecap}{}", self.flat_branch(eb)));
                }
                s
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
                let mut s = String::new();
                if let Some(l) = label {
                    s.push_str(&format!("{l}: "));
                }
                if *is_inline {
                    s.push_str("inline ");
                }
                let ops = operands
                    .iter()
                    .map(|o| self.flat_for_operand(o))
                    .collect::<Vec<_>>()
                    .join(", ");
                let caps = captures
                    .iter()
                    .map(capture_name_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                s.push_str(&format!("for ({ops}) |{caps}| {}", self.flat_branch(body)));
                if let Some(eb) = else_branch {
                    s.push_str(&format!(" else {}", self.flat_branch(eb)));
                }
                s
            }
            _ => unreachable!("flat_control_flow on a non-control-flow expression"),
        }
    }

    /// Renders a control-flow branch body flat (blocks force a break).
    fn flat_branch(&self, body: &Expr) -> String {
        match body {
            Expr::Block { body: stmts, .. } if !stmts.is_empty() => "{\n}".into(),
            Expr::Block { .. } => "{}".into(),
            other => self.flat_expr(other, 0),
        }
    }

    /// Renders a `while` continue clause / inline statement flat.
    fn flat_stmt_inline(&self, s: &Stmt) -> String {
        match s {
            Stmt::Expr { expr, .. } => self.flat_expr(expr, 0),
            Stmt::Assign {
                target, op, value, ..
            } => format!(
                "{} {} {}",
                self.flat_expr(target, 0),
                assign_op_str(*op),
                self.flat_expr(value, 0)
            ),
            _ => String::new(),
        }
    }

    /// Renders a switch pattern flat.
    fn flat_switch_pattern(&self, p: &SwitchPattern) -> String {
        match p {
            SwitchPattern::Else => "else".into(),
            SwitchPattern::Items(items) => items
                .iter()
                .map(|i| self.flat_switch_item(i))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Renders one switch item flat; an inclusive range prints tight (`0...9`).
    fn flat_switch_item(&self, i: &SwitchItem) -> String {
        match &i.hi {
            Some(hi) => format!("{}...{}", self.flat_expr(&i.lo, 0), self.flat_expr(hi, 0)),
            None => self.flat_expr(&i.lo, 0),
        }
    }
}

// =====================================================================
//  Free helpers
// =====================================================================

/// Which side of a parent operator a child sits on, for parenthesization.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Postfix,
}

/// How a control-flow *then*/loop branch was emitted, so the following `else`
/// (if any) knows how to attach itself without producing unparseable output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchTerm {
    /// The branch ended in a lone `}` line — an `else` joins it as `} else …`.
    Braced,
    /// The branch is a brace-less body whose last line is NOT a closing `}` (a
    /// fit-on-one-line `if (c) x` or a wrapped brace-less value branch). The
    /// `else` must start on its own fresh line (`} else` joining would be wrong
    /// and emitting `;` before the `else` produces unparseable orphaned-`else`
    /// output).
    Braceless,
}

/// `true` if `e` is a block expression (bare or labeled).
fn is_block_expr(e: &Expr) -> bool {
    matches!(e, Expr::Block { .. })
}

/// `true` if `c` can appear in an identifier (letter, digit, or `_`).
fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// `true` if `e` is a binary expression (so it can be greedily wrapped at its
/// operators when too long).
fn is_flat_binary_chain(e: &Expr) -> bool {
    matches!(e, Expr::Binary { .. })
}

/// Flattens a same-operator, left-associative binary chain into its operands (in
/// source order) and the operators between them. A nested binary of the SAME
/// operator on the left is unrolled; any other operand stays whole. Returns
/// `(operands, operators)` with `operands.len() == operators.len() + 1`.
fn flatten_binary_chain(e: &Expr) -> (Vec<&Expr>, Vec<&'static str>) {
    let mut operands: Vec<&Expr> = Vec::new();
    let mut ops: Vec<&'static str> = Vec::new();
    // Determine the chain operator from the outermost node.
    let chain_op = match e {
        Expr::Binary { op, .. } => *op,
        _ => {
            return (vec![e], Vec::new());
        }
    };
    // Walk the left spine, collecting same-operator links.
    fn walk<'a>(
        e: &'a Expr,
        chain_op: BinOp,
        operands: &mut Vec<&'a Expr>,
        ops: &mut Vec<&'static str>,
    ) {
        if let Expr::Binary { op, lhs, rhs, .. } = e {
            if *op == chain_op {
                walk(lhs, chain_op, operands, ops);
                ops.push(bin_op_str(*op));
                operands.push(rhs);
                return;
            }
        }
        operands.push(e);
    }
    walk(e, chain_op, &mut operands, &mut ops);
    (operands, ops)
}

/// The precedence level of an expression (higher binds tighter). Mirrors the
/// parser's cascade: orelse/catch=1 … postfix/primary=12.
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Binary { op, .. } => match op {
            BinOp::Orelse => 1,
            BinOp::Or => 2,
            BinOp::And => 3,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::BitOr | BinOp::ErrSetMerge => 5,
            BinOp::BitXor => 6,
            BinOp::BitAnd => 7,
            BinOp::Shl | BinOp::Shr => 8,
            BinOp::Add | BinOp::Sub | BinOp::Concat => 9,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 10,
        },
        Expr::Catch { .. } => 1,
        Expr::Unary { .. } | Expr::Comptime { .. } => 11,
        Expr::Optional { .. }
        | Expr::Pointer { .. }
        | Expr::Slice { .. }
        | Expr::ArrayType { .. }
        | Expr::ErrorUnion { .. } => 11,
        // Postfix and primaries bind tightest.
        _ => 12,
    }
}

/// Whether `child` needs parentheses on the `side` of `parent`.
fn needs_parens(parent: &Expr, child: &Expr, side: Side) -> bool {
    let pp = prec(parent);
    let cp = prec(child);
    match side {
        Side::Postfix | Side::Right => match parent {
            // For a binary parent's right operand (and any postfix base), a
            // strictly-lower-precedence child needs parens; an equal-precedence
            // child needs parens on the right of a left-associative operator.
            Expr::Binary { .. } | Expr::Catch { .. } => {
                if side == Side::Right {
                    cp <= pp
                } else {
                    cp < pp
                }
            }
            // Prefix/postfix/type parents: a lower-precedence child needs parens.
            _ => cp < pp,
        },
        Side::Left => {
            // Left operand of a left-associative binary: equal precedence is OK;
            // only strictly lower needs parens.
            cp < pp
        }
    }
}

/// The textual form of a binary operator (spaces are added by the caller).
fn bin_op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Concat => "++",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Orelse => "orelse",
        BinOp::ErrSetMerge => "||",
    }
}

/// The textual form of an assignment operator.
fn assign_op_str(op: k2_syntax::AssignOp) -> &'static str {
    use k2_syntax::AssignOp::*;
    match op {
        Eq => "=",
        AddEq => "+=",
        SubEq => "-=",
        MulEq => "*=",
        DivEq => "/=",
        RemEq => "%=",
        AndEq => "&=",
        OrEq => "|=",
        XorEq => "^=",
        ShlEq => "<<=",
        ShrEq => ">>=",
    }
}

/// The keyword/tag clause that opens a container body (e.g. `struct`,
/// `enum(u8)`, `union(enum)`, `extern struct`).
fn container_keyword(kind: &ContainerKind, p: &Printer) -> String {
    match kind {
        ContainerKind::Struct { is_extern } => {
            if *is_extern {
                "extern struct".into()
            } else {
                "struct".into()
            }
        }
        ContainerKind::Enum { tag } => match tag {
            Some(t) => format!("enum({})", p.flat_expr(t, 0)),
            None => "enum".into(),
        },
        ContainerKind::Union { tag } => match tag {
            UnionTag::None => "union".into(),
            UnionTag::Inferred => "union(enum)".into(),
            UnionTag::Typed(t) => format!("union({})", p.flat_expr(t, 0)),
        },
    }
}

/// Renders a `|cap, ...|` capture clause.
fn capture_str(c: &Capture) -> String {
    let names = c
        .names
        .iter()
        .map(capture_name_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("|{names}|")
}

/// Renders one capture name (`*name` for by-ref).
fn capture_name_str(n: &CaptureName) -> String {
    if n.by_ref {
        format!("*{}", n.name)
    } else {
        n.name.clone()
    }
}

/// Splits a stored doc comment (a single `String`, internally joined with `\n`
/// by the parser) back into its lines, each already carrying its `///` prefix,
/// trimming trailing whitespace.
fn doc_lines(doc: &str) -> Vec<String> {
    doc.split('\n').map(|l| l.trim_end().to_string()).collect()
}

/// The doc comment of an item, if any.
fn item_doc(item: &Item) -> Option<&str> {
    match item {
        Item::Const { doc, .. } | Item::Var { doc, .. } | Item::Fn { doc, .. } => doc.as_deref(),
        Item::Test { doc, .. } => doc.as_deref(),
        Item::Comptime { .. } => None,
    }
}

/// The 1-based line an expression begins on (for trailing-comment matching).
fn line_of(e: &Expr) -> u32 {
    e.span().line
}
