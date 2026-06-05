//! `textDocument/inlayHint`: inline type hints for un-annotated bindings and
//! parameter-name hints at call sites, within a requested range.
//!
//! Two hint families, both read from the type checker's side tables:
//!
//! * **Type hints** — for each `const`/`var` *without* an explicit type
//!   annotation, the inferred type (`binding_types[def]`) is shown just after the
//!   binding name as `: T`. An already-annotated binding gets no hint (the type is
//!   already visible).
//! * **Parameter-name hints** — at each call argument, if the callee resolves to a
//!   [`FnSig`](k2_types::FnSig), the parameter name is shown before the argument as
//!   `name:`. Skipped when the argument is itself that identifier (no value added).
//!
//! Only hints whose anchor falls inside the requested `[lo, hi)` range are
//! returned; positions are produced through the [`PositionMap`](crate::position::PositionMap)
//! so they land exactly in UTF-16 terms.

use k2_syntax::{Expr, Item, Stmt};
use k2_types::{FnSig, MemberRes, Type, Typed};

use crate::analysis::Analysis;
use crate::features::use_at_offset;
use crate::json::JsonValue;

/// LSP `InlayHintKind` values.
const KIND_TYPE: i64 = 1;
const KIND_PARAMETER: i64 = 2;

/// One pending inlay hint before JSON encoding.
struct Hint {
    /// The scalar anchor offset (a position in the buffer).
    offset: u32,
    label: String,
    kind: i64,
    padding_left: bool,
    padding_right: bool,
}

/// Computes the `InlayHint[]` in `[lo, hi)` (scalar offsets) for `analysis`.
pub fn compute(analysis: &Analysis, lo: u32, hi: u32) -> JsonValue {
    let typed = match &analysis.typed {
        Some(t) => t,
        None => return JsonValue::arr(Vec::new()),
    };
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::arr(Vec::new()),
    };

    let mut hints: Vec<Hint> = Vec::new();

    // --- Type hints for un-annotated const/var bindings -------------------
    // Each binding name span is recorded as a `Def`; we cross-reference the AST to
    // learn whether the declaration carried an explicit type annotation.
    for item in &analysis.parse.file.items {
        collect_binding_type_hints(typed, resolved, item, lo, hi, &mut hints);
    }

    // --- Parameter-name hints at call sites -------------------------------
    let mut calls: Vec<&Expr> = Vec::new();
    for item in &analysis.parse.file.items {
        walk_item(item, &mut |e| {
            if matches!(e, Expr::Call { .. }) {
                calls.push(e);
            }
        });
    }
    for call in calls {
        if let Expr::Call { callee, args, .. } = call {
            if let Some(sig) = resolve_callee_sig(analysis, typed, callee) {
                for (i, arg) in args.iter().enumerate() {
                    let arg_span = arg.span();
                    if arg_span.start < lo || arg_span.start > hi {
                        continue;
                    }
                    let param = match sig.params.get(i) {
                        Some(p) => p,
                        None => break, // varargs tail — no named params left.
                    };
                    if param.name.is_empty() || param.name == "_" {
                        continue;
                    }
                    // Skip when the argument is literally the parameter name.
                    if let Expr::Ident { name, .. } = arg {
                        if name == &param.name {
                            continue;
                        }
                    }
                    hints.push(Hint {
                        offset: arg_span.start,
                        label: format!("{}:", param.name),
                        kind: KIND_PARAMETER,
                        padding_left: false,
                        padding_right: true,
                    });
                }
            }
        }
    }

    let items: Vec<JsonValue> = hints
        .into_iter()
        .map(|h| {
            let mut pairs = vec![
                (
                    "position",
                    analysis.posmap.offset_to_position_json(h.offset),
                ),
                ("label", JsonValue::str(h.label)),
                ("kind", JsonValue::num(h.kind)),
            ];
            if h.padding_left {
                pairs.push(("paddingLeft", JsonValue::Bool(true)));
            }
            if h.padding_right {
                pairs.push(("paddingRight", JsonValue::Bool(true)));
            }
            JsonValue::obj(pairs)
        })
        .collect();
    JsonValue::arr(items)
}

/// Emits a type hint for an un-annotated `const`/`var` whose name span is in
/// range, recursing into function/test/comptime bodies for block-local bindings.
fn collect_binding_type_hints(
    typed: &Typed,
    resolved: &k2_resolve::Resolved,
    item: &Item,
    lo: u32,
    hi: u32,
    hints: &mut Vec<Hint>,
) {
    match item {
        Item::Const { name, ty, span, .. } => {
            if ty.is_none() {
                push_type_hint(typed, resolved, name, *span, lo, hi, hints);
            }
        }
        Item::Var { name, ty, span, .. } => {
            if ty.is_none() {
                push_type_hint(typed, resolved, name, *span, lo, hi, hints);
            }
        }
        Item::Fn {
            body: Some(body), ..
        } => {
            for s in body {
                collect_stmt_type_hints(typed, resolved, s, lo, hi, hints);
            }
        }
        Item::Test { body, .. } | Item::Comptime { body, .. } => {
            for s in body {
                collect_stmt_type_hints(typed, resolved, s, lo, hi, hints);
            }
        }
        _ => {}
    }
}

/// Emits a type hint for an un-annotated `const`/`var` *statement* (block-local),
/// recursing into nested blocks.
fn collect_stmt_type_hints(
    typed: &Typed,
    resolved: &k2_resolve::Resolved,
    stmt: &Stmt,
    lo: u32,
    hi: u32,
    hints: &mut Vec<Hint>,
) {
    match stmt {
        Stmt::Const { name, ty, span, .. } => {
            if ty.is_none() {
                push_type_hint(typed, resolved, name, *span, lo, hi, hints);
            }
        }
        Stmt::Var { name, ty, span, .. } => {
            if ty.is_none() {
                push_type_hint(typed, resolved, name, *span, lo, hi, hints);
            }
        }
        Stmt::Defer { body, .. } | Stmt::Errdefer { body, .. } => {
            collect_stmt_type_hints(typed, resolved, body, lo, hi, hints)
        }
        Stmt::Comptime { body, .. } | Stmt::Block { body, .. } => {
            for s in body {
                collect_stmt_type_hints(typed, resolved, s, lo, hi, hints);
            }
        }
        _ => {}
    }
}

/// Looks up the declaration `Def` by its name + statement span, and if the
/// inferred type is known, pushes a `: T` hint just after the binding name.
fn push_type_hint(
    typed: &Typed,
    resolved: &k2_resolve::Resolved,
    name: &str,
    decl_span: k2_syntax::Span,
    lo: u32,
    hi: u32,
    hints: &mut Vec<Hint>,
) {
    // The binding `Def` is the one whose name matches and whose own span sits
    // inside this declaration statement (the name token, not the whole stmt).
    let def = resolved.defs.iter().find(|d| {
        d.name == name
            && d.span.start >= decl_span.start
            && d.span.end <= decl_span.end
            && d.span.end > d.span.start
    });
    let def = match def {
        Some(d) => d,
        None => return,
    };
    if def.span.end < lo || def.span.end > hi {
        return;
    }
    let tid = match typed.binding_types.get(&def.id) {
        Some(&t) => t,
        None => return,
    };
    hints.push(Hint {
        offset: def.span.end,
        label: format!(": {}", typed.arena.fmt(tid)),
        kind: KIND_TYPE,
        padding_left: false,
        padding_right: false,
    });
}

/// Resolves a call `callee` to its [`FnSig`] (bare fn or method). Shared shape
/// with the signature-help provider.
fn resolve_callee_sig(analysis: &Analysis, typed: &Typed, callee: &Expr) -> Option<FnSig> {
    let resolved = analysis.resolved.as_ref()?;
    let sig_id = match callee {
        Expr::Ident { span, .. } => {
            let use_ = use_at_offset(resolved, span.start)?;
            let id = match use_.res {
                k2_resolve::Resolution::Def(id)
                | k2_resolve::Resolution::Predeclared(id)
                | k2_resolve::Resolution::Module(id) => id,
                _ => return None,
            };
            let tid = *typed.binding_types.get(&id)?;
            fn_sig_id(typed, tid)?
        }
        Expr::Field { span, .. } => {
            let member = typed.members.get(&(span.start, span.end))?;
            match *member {
                MemberRes::Decl(def_id) => {
                    let tid = *typed.binding_types.get(&def_id)?;
                    fn_sig_id(typed, tid)?
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    typed.arena.fnsigs.get(sig_id as usize).cloned()
}

/// Extracts the `FnSigId` index from a [`Type::Fn`].
fn fn_sig_id(typed: &Typed, tid: k2_types::TypeId) -> Option<u32> {
    match typed.arena.get(tid) {
        Type::Fn(f) => Some(f.0),
        _ => None,
    }
}

/// Visits every expression in an item, invoking `f` on each (pre-order).
fn walk_item<'a>(item: &'a Item, f: &mut impl FnMut(&'a Expr)) {
    match item {
        Item::Const { value, .. } => walk_expr(value, f),
        Item::Var { value: Some(v), .. } => walk_expr(v, f),
        Item::Fn {
            body: Some(body), ..
        } => walk_stmts(body, f),
        Item::Test { body, .. } | Item::Comptime { body, .. } => walk_stmts(body, f),
        _ => {}
    }
}

/// Visits every expression in a statement list (pre-order).
fn walk_stmts<'a>(stmts: &'a [Stmt], f: &mut impl FnMut(&'a Expr)) {
    for s in stmts {
        match s {
            Stmt::Const { value, .. } => walk_expr(value, f),
            Stmt::Var { value: Some(v), .. } => walk_expr(v, f),
            Stmt::Return { value: Some(v), .. } => walk_expr(v, f),
            Stmt::Expr { expr, .. } => walk_expr(expr, f),
            Stmt::Assign { target, value, .. } => {
                walk_expr(target, f);
                walk_expr(value, f);
            }
            Stmt::Defer { body, .. } | Stmt::Errdefer { body, .. } => {
                walk_stmts(std::slice::from_ref(body), f)
            }
            Stmt::Comptime { body, .. } | Stmt::Block { body, .. } => walk_stmts(body, f),
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => walk_expr(expr, f),
            Stmt::Break { value: Some(v), .. } => walk_expr(v, f),
            _ => {}
        }
    }
}

/// Visits `e` and every sub-expression (pre-order), invoking `f`.
fn walk_expr<'a>(e: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    f(e);
    for child in children(e) {
        walk_expr(child, f);
    }
}

/// The direct sub-expressions of `e` (same traversal shape used by signatureHelp).
fn children(e: &Expr) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    match e {
        Expr::Builtin { args, .. } => out.extend(args.iter()),
        Expr::Field { base, .. } => out.push(base),
        Expr::Call { callee, args, .. } => {
            out.push(callee);
            out.extend(args.iter());
        }
        Expr::Binary { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        Expr::Unary { operand, .. } => out.push(operand),
        Expr::Optional { inner, .. }
        | Expr::Pointer { inner, .. }
        | Expr::Slice { inner, .. }
        | Expr::ManyPtr { inner, .. }
        | Expr::Comptime { inner, .. } => out.push(inner),
        Expr::ArrayType { len, inner, .. } => {
            out.push(len);
            out.push(inner);
        }
        Expr::ErrorUnion { err, ok, .. } => {
            if let Some(err) = err {
                out.push(err);
            }
            out.push(ok);
        }
        Expr::Init { ty, body, .. } => {
            if let Some(ty) = ty {
                out.push(ty);
            }
            match body {
                k2_syntax::InitBody::Fields(fields) => out.extend(fields.iter().map(|f| &f.value)),
                k2_syntax::InitBody::Tuple(elems) => out.extend(elems.iter()),
            }
        }
        Expr::Index { base, index, .. } => {
            out.push(base);
            out.push(index);
        }
        Expr::SliceExpr { base, lo, hi, .. } => {
            out.push(base);
            out.push(lo);
            if let Some(hi) = hi {
                out.push(hi);
            }
        }
        Expr::Deref { base, .. } | Expr::Unwrap { base, .. } => out.push(base),
        Expr::Catch { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        Expr::Block { body, .. } => {
            let mut tmp: Vec<&Expr> = Vec::new();
            collect_block_exprs(body, &mut tmp);
            out.extend(tmp);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            out.push(cond);
            out.push(then_branch);
            if let Some(eb) = else_branch {
                out.push(eb);
            }
        }
        Expr::While {
            cond,
            body,
            else_branch,
            ..
        } => {
            out.push(cond);
            out.push(body);
            if let Some(eb) = else_branch {
                out.push(eb);
            }
        }
        Expr::For {
            operands,
            body,
            else_branch,
            ..
        } => {
            for op in operands {
                match op {
                    k2_syntax::ForOperand::Value(e) => out.push(e),
                    k2_syntax::ForOperand::Range { lo, hi, .. } => {
                        out.push(lo);
                        if let Some(hi) = hi {
                            out.push(hi);
                        }
                    }
                }
            }
            out.push(body);
            if let Some(eb) = else_branch {
                out.push(eb);
            }
        }
        Expr::Switch {
            scrutinee, arms, ..
        } => {
            out.push(scrutinee);
            for arm in arms {
                out.push(&arm.body);
            }
        }
        _ => {}
    }
    out
}

/// Collects the top-level expressions reachable from a block's statements (so a
/// block-as-expression still contributes its call sites).
fn collect_block_exprs<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a Expr>) {
    for s in stmts {
        match s {
            Stmt::Const { value, .. } => out.push(value),
            Stmt::Var { value: Some(v), .. } => out.push(v),
            Stmt::Return { value: Some(v), .. } => out.push(v),
            Stmt::Expr { expr, .. } => out.push(expr),
            Stmt::Assign { target, value, .. } => {
                out.push(target);
                out.push(value);
            }
            Stmt::Comptime { body, .. } | Stmt::Block { body, .. } => {
                collect_block_exprs(body, out)
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => out.push(expr),
            Stmt::Break { value: Some(v), .. } => out.push(v),
            _ => {}
        }
    }
}
