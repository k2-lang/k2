//! `textDocument/signatureHelp`: the callee's signature and the active parameter
//! at a call site.
//!
//! At `callee(a, b, |cursor|)` the provider finds the innermost `Expr::Call`
//! whose span covers the cursor, resolves the `callee` to a
//! [`FnSig`](k2_types::FnSig) (via the resolver's `Use` → `DefId` →
//! `binding_types`, or the type checker's member table for a method call), and
//! reports the active parameter as the number of *top-level* commas between the
//! opening paren and the cursor — commas nested in inner calls / array / struct
//! literals are skipped by tracking bracket depth and string/char literals.
//!
//! The signature `label` is rendered as `fn(p0: T0, p1: T1) Ret`, and each
//! parameter records its `[start, end)` UTF-16 offsets *into that label* so the
//! editor can underline the active parameter precisely.

use k2_syntax::{Expr, SourceFile, Stmt};
use k2_types::{FnSig, MemberRes, Type, Typed};

use crate::analysis::Analysis;
use crate::features::use_at_offset;
use crate::json::JsonValue;

/// Computes the `SignatureHelp` at scalar `offset`, or `JsonValue::Null` when the
/// cursor is not inside a call whose callee resolves to a function.
pub fn compute(analysis: &Analysis, offset: u32) -> JsonValue {
    let typed = match &analysis.typed {
        Some(t) => t,
        None => return JsonValue::Null,
    };

    // The innermost call covering the cursor.
    let call = match innermost_call(&analysis.parse.file, offset) {
        Some(c) => c,
        None => return JsonValue::Null,
    };
    let (callee, _args, call_span) = match call {
        Expr::Call { callee, args, span } => (callee.as_ref(), args, *span),
        _ => return JsonValue::Null,
    };

    let sig = match resolve_callee_sig(analysis, typed, callee) {
        Some(s) => s,
        None => return JsonValue::Null,
    };

    // The opening paren is the first '(' at or after the callee's end within the
    // call span; commas after it (at depth 0) advance the active parameter.
    let chars: Vec<char> = analysis.source.chars().collect();
    let open_paren = match find_open_paren(&chars, callee.span().end, call_span.end) {
        Some(p) => p,
        None => return JsonValue::Null,
    };
    // Clamp the cursor into the argument region so a caret on the closing paren
    // still counts the commas before it.
    let cursor = offset.min(call_span.end);
    let active = active_param(&chars, open_paren + 1, cursor);
    // Clamp to the last parameter (or one past, for a varargs tail).
    let max_index = if sig.is_varargs {
        sig.params.len() as u32
    } else {
        sig.params.len().saturating_sub(1) as u32
    };
    let active = active.min(max_index);

    let (label, param_ranges) = render_signature(typed, &sig);
    let parameters: Vec<JsonValue> = param_ranges
        .into_iter()
        .map(|(start, end)| {
            JsonValue::obj(vec![(
                "label",
                JsonValue::arr(vec![
                    JsonValue::num(i64::from(start)),
                    JsonValue::num(i64::from(end)),
                ]),
            )])
        })
        .collect();

    let signature = JsonValue::obj(vec![
        ("label", JsonValue::str(label)),
        ("parameters", JsonValue::arr(parameters)),
    ]);
    JsonValue::obj(vec![
        ("signatures", JsonValue::arr(vec![signature])),
        ("activeSignature", JsonValue::num(0)),
        ("activeParameter", JsonValue::num(i64::from(active))),
    ])
}

/// Finds the innermost `Expr::Call` whose span covers `offset`, walking the whole
/// file. The narrowest covering call wins, so nested `f(g(|here|))` selects `g`.
fn innermost_call(file: &SourceFile, offset: u32) -> Option<&Expr> {
    let mut best: Option<&Expr> = None;
    for item in &file.items {
        walk_item(item, offset, &mut best);
    }
    best
}

/// Considers `e` (and recurses into it), updating `best` with the narrowest
/// `Expr::Call` whose span covers `offset`.
fn walk_expr<'a>(e: &'a Expr, offset: u32, best: &mut Option<&'a Expr>) {
    if let Expr::Call { span, .. } = e {
        if span.start <= offset && offset <= span.end {
            let better = match best {
                None => true,
                Some(b) => span.end - span.start < b.span().end - b.span().start,
            };
            if better {
                *best = Some(e);
            }
        }
    }
    for child in children(e) {
        walk_expr(child, offset, best);
    }
}

/// The direct sub-expressions of `e`, used to drive the recursive walk. Returns
/// owned references collected into a `Vec` for a uniform iteration shape.
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
        Expr::Block { body, .. } => collect_stmts(body, &mut out),
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

/// Pushes every expression reachable from a list of statements onto `out`.
fn collect_stmts<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a Expr>) {
    for s in stmts {
        match s {
            Stmt::Const { value, .. } => out.push(value),
            Stmt::Var { value, .. } => {
                if let Some(v) = value {
                    out.push(v);
                }
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    out.push(v);
                }
            }
            Stmt::Expr { expr, .. } => out.push(expr),
            Stmt::Assign { target, value, .. } => {
                out.push(target);
                out.push(value);
            }
            Stmt::Defer { body, .. } | Stmt::Errdefer { body, .. } => {
                collect_stmts(std::slice::from_ref(body), out)
            }
            Stmt::Comptime { body, .. } | Stmt::Block { body, .. } => collect_stmts(body, out),
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => out.push(expr),
            Stmt::Break { value, .. } => {
                if let Some(v) = value {
                    out.push(v);
                }
            }
            Stmt::Continue { .. } => {}
        }
    }
}

/// Walks an item's bodies looking for the innermost covering call.
fn walk_item<'a>(item: &'a k2_syntax::Item, offset: u32, best: &mut Option<&'a Expr>) {
    use k2_syntax::Item;
    match item {
        Item::Const { value, .. } => walk_expr(value, offset, best),
        Item::Var { value, .. } => {
            if let Some(v) = value {
                walk_expr(v, offset, best);
            }
        }
        Item::Fn { body, .. } => {
            if let Some(body) = body {
                let mut exprs: Vec<&Expr> = Vec::new();
                collect_stmts(body, &mut exprs);
                for e in exprs {
                    walk_expr(e, offset, best);
                }
            }
        }
        Item::Test { body, .. } | Item::Comptime { body, .. } => {
            let mut exprs: Vec<&Expr> = Vec::new();
            collect_stmts(body, &mut exprs);
            for e in exprs {
                walk_expr(e, offset, best);
            }
        }
    }
}

/// Resolves a call's `callee` expression to its [`FnSig`], whether it is a bare
/// function identifier or a method (`base.method`) access.
fn resolve_callee_sig(analysis: &Analysis, typed: &Typed, callee: &Expr) -> Option<FnSig> {
    let resolved = analysis.resolved.as_ref()?;
    // A bare identifier callee resolves through the Uses map to a binding type.
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
            // A method call: the checker recorded a member resolution at the
            // access span; a `Decl` carries the method's DefId.
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

/// Extracts the `FnSigId` index from a [`Type::Fn`], or `None` for a non-callable.
fn fn_sig_id(typed: &Typed, tid: k2_types::TypeId) -> Option<u32> {
    match typed.arena.get(tid) {
        Type::Fn(f) => Some(f.0),
        _ => None,
    }
}

/// Finds the opening paren of a call: the first `'('` in `[after_callee, end)`.
fn find_open_paren(chars: &[char], after_callee: u32, end: u32) -> Option<u32> {
    let hi = (end as usize).min(chars.len());
    (after_callee as usize..hi)
        .find(|&i| chars[i] == '(')
        .map(|i| i as u32)
}

/// Counts the number of top-level commas in `[lo, hi)`, i.e. the active-parameter
/// index. Commas nested inside parentheses/brackets/braces, or inside a string or
/// character literal, are not counted.
fn active_param(chars: &[char], lo: u32, hi: u32) -> u32 {
    let mut depth: i32 = 0;
    let mut commas: u32 = 0;
    let mut in_str = false;
    let mut in_char = false;
    let hi = (hi as usize).min(chars.len());
    let mut i = lo as usize;
    while i < hi {
        let c = chars[i];
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if in_char {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '\'' => in_char = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => commas += 1,
                _ => {}
            }
        }
        i += 1;
    }
    commas
}

/// Renders a function signature as `fn(p0: T0, p1: T1) Ret`, returning the label
/// and, for each parameter, its `[start, end)` UTF-16 code-unit offsets into the
/// label string (so the editor can highlight the active parameter exactly).
fn render_signature(typed: &Typed, sig: &FnSig) -> (String, Vec<(u32, u32)>) {
    let mut label = String::from("fn(");
    let mut start_u16 = utf16_len(&label);
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for (i, p) in sig.params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
            start_u16 = utf16_len(&label);
        }
        let name = if p.name.is_empty() { "_" } else { &p.name };
        let piece = format!("{}: {}", name, typed.arena.fmt(p.ty));
        label.push_str(&piece);
        let end_u16 = utf16_len(&label);
        ranges.push((start_u16, end_u16));
    }
    label.push_str(") ");
    label.push_str(&typed.arena.fmt(sig.ret));
    (label, ranges)
}

/// The number of UTF-16 code units in `s`, matching LSP label-offset semantics.
fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}
