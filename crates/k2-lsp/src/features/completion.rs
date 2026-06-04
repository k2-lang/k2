//! `textDocument/completion`: scope-aware identifier completions, plus member
//! completions after `.` when the base type is known.
//!
//! Two modes are selected by looking at the character immediately before the
//! cursor (skipping the partial identifier the user has begun typing):
//!
//! * **Member completion** — the text before the prefix ends with `.`: the base
//!   expression's type drives the candidate list (fields, methods, variants).
//! * **Scope-aware completion** — otherwise: every binding visible at the cursor
//!   (params, locals declared so far, file items, and the always-visible
//!   predeclared names), filtered by the partial prefix.
//!
//! ## Locating the enclosing scope without scope spans
//!
//! Some resolver scopes (a function's param and top body block) are created with
//! a zero `span`, so a naive span-cover walk would miss params/locals. Instead
//! each scope's *effective* extent is derived from the spans of the definitions
//! it (and its descendants) own — param/local def sites bracket the function
//! body precisely — and the deepest scope whose effective extent covers the
//! cursor is chosen. The file and predeclared roots are always included.

use std::collections::HashSet;

use k2_resolve::{DefKind, Resolved, ScopeId};
use k2_types::Type;

use crate::analysis::Analysis;
use crate::json::JsonValue;

// LSP `CompletionItemKind` constants used here.
const KIND_METHOD: i64 = 2;
const KIND_FUNCTION: i64 = 3;
const KIND_FIELD: i64 = 5;
const KIND_VARIABLE: i64 = 6;
const KIND_MODULE: i64 = 9;
const KIND_KEYWORD: i64 = 14;
const KIND_ENUM_MEMBER: i64 = 20;

/// Computes the completion result at scalar `offset`.
pub fn compute(analysis: &Analysis, offset: u32) -> JsonValue {
    let chars: Vec<char> = analysis.source.chars().collect();
    let prefix_start = identifier_start(&chars, offset);
    let items = if after_dot(&chars, prefix_start) {
        member_items(analysis, prefix_start)
    } else {
        scope_items(analysis, offset, &chars, prefix_start)
    };
    JsonValue::obj(vec![
        ("isIncomplete", JsonValue::Bool(false)),
        ("items", JsonValue::arr(items)),
    ])
}

/// The scalar offset where the partial identifier under/just-before the cursor
/// begins (the cursor may sit just past the last typed character).
fn identifier_start(chars: &[char], offset: u32) -> u32 {
    let mut i = offset as usize;
    while i > 0 && is_ident_char(chars[i - 1]) {
        i -= 1;
    }
    i as u32
}

/// `true` if `c` may appear in a k2 identifier.
fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// `true` if the character immediately before `prefix_start` (skipping spaces)
/// is a `.`, signalling member completion.
fn after_dot(chars: &[char], prefix_start: u32) -> bool {
    let mut i = prefix_start as usize;
    while i > 0 && (chars[i - 1] == ' ' || chars[i - 1] == '\t') {
        i -= 1;
    }
    i > 0 && chars[i - 1] == '.'
}

/// The partial identifier text already typed before the cursor.
fn prefix_text(chars: &[char], prefix_start: u32, offset: u32) -> String {
    chars[prefix_start as usize..offset as usize]
        .iter()
        .collect()
}

// =========================================================================
//  Scope-aware identifier completion
// =========================================================================

/// Builds the visible-identifier candidate list at `offset`.
fn scope_items(
    analysis: &Analysis,
    offset: u32,
    chars: &[char],
    prefix_start: u32,
) -> Vec<JsonValue> {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return Vec::new(),
    };
    let prefix = prefix_text(chars, prefix_start, offset);

    let innermost = innermost_scope(resolved, offset);
    let mut seen: HashSet<String> = HashSet::new();
    let mut items: Vec<JsonValue> = Vec::new();

    // Walk from the innermost scope outward to the predeclared root, unioning
    // each scope's directly-declared names (nearer scopes shadow farther ones).
    let mut scope = Some(innermost);
    while let Some(sid) = scope {
        let s = &resolved.scopes[sid.index()];
        for (name, def_id) in &s.names {
            let def = &resolved.defs[def_id.index()];
            // Fields are only reachable as `self.field`, never bare.
            if def.kind == DefKind::Field {
                continue;
            }
            // k2 block locals are order-dependent: a `const`/`var` is in scope
            // only *after* its declaration. Skip a local declared at or after the
            // cursor so completion never offers a name the resolver would reject
            // as undeclared (and never a self-reference like `const a = a;`).
            // Params, items, captures, modules, and predeclared names are
            // order-independent, so they are not filtered.
            if def.kind == DefKind::Local && def.span.start >= offset {
                continue;
            }
            if !prefix.is_empty() && !name.starts_with(&prefix) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            items.push(completion_item(analysis, name, def.kind, *def_id));
        }
        scope = s.parent;
    }
    items
}

/// Finds the deepest scope whose effective extent covers `offset`, defaulting to
/// the file scope. The effective extent of a scope is the union of its own span
/// (when non-zero) and the spans of every definition it and its descendants own.
fn innermost_scope(resolved: &Resolved, offset: u32) -> ScopeId {
    // Precompute each scope's effective [start, end) from its defs.
    let mut extents: Vec<Option<(u32, u32)>> = vec![None; resolved.scopes.len()];
    for def in &resolved.defs {
        if def.span.end == 0 && def.span.start == 0 {
            continue; // predeclared / synthetic
        }
        widen(
            &mut extents,
            def.scope.index(),
            def.span.start,
            def.span.end,
        );
    }
    // Also fold in scopes' own recorded spans (containers/blocks with real spans).
    for s in resolved.scopes.iter() {
        if s.span.end > s.span.start {
            widen(&mut extents, s.id.index(), s.span.start, s.span.end);
        }
    }
    // Propagate child extents up to parents so an enclosing scope covers its
    // descendants' ranges.
    let mut changed = true;
    while changed {
        changed = false;
        for s in resolved.scopes.iter() {
            if let (Some(parent), Some((cs, ce))) = (s.parent, extents[s.id.index()]) {
                let before = extents[parent.index()];
                widen(&mut extents, parent.index(), cs, ce);
                if extents[parent.index()] != before {
                    changed = true;
                }
            }
        }
    }

    // The file scope is the default. Then descend to the deepest covering scope.
    let mut best = resolved.file_scope;
    let mut best_width = u32::MAX;
    for s in resolved.scopes.iter() {
        if let Some((start, end)) = extents[s.id.index()] {
            if start <= offset && offset <= end {
                let width = end - start;
                if width <= best_width {
                    best = s.id;
                    best_width = width;
                }
            }
        }
    }
    best
}

/// Widens `extents[i]` to include `[start, end)`.
fn widen(extents: &mut [Option<(u32, u32)>], i: usize, start: u32, end: u32) {
    extents[i] = Some(match extents[i] {
        None => (start, end),
        Some((s, e)) => (s.min(start), e.max(end)),
    });
}

/// Builds one scope-completion item from a definition.
fn completion_item(
    analysis: &Analysis,
    name: &str,
    kind: DefKind,
    def_id: k2_resolve::DefId,
) -> JsonValue {
    let lsp_kind = match kind {
        DefKind::Item => KIND_FUNCTION,
        DefKind::Param | DefKind::Local | DefKind::Capture => KIND_VARIABLE,
        DefKind::Module => KIND_MODULE,
        DefKind::Predeclared => KIND_KEYWORD,
        DefKind::Field => KIND_FIELD,
    };
    let detail = analysis
        .typed
        .as_ref()
        .and_then(|t| t.binding_types.get(&def_id).map(|&tid| t.arena.fmt(tid)));
    item(name, lsp_kind, detail)
}

// =========================================================================
//  Member completion (after `.`)
// =========================================================================

/// Builds the member candidate list for a `base.` access, where the cursor's
/// prefix starts at `prefix_start` (so the base ends just before the `.`).
fn member_items(analysis: &Analysis, prefix_start: u32) -> Vec<JsonValue> {
    let typed = match &analysis.typed {
        Some(t) => t,
        None => return Vec::new(),
    };
    let chars: Vec<char> = analysis.source.chars().collect();
    // Locate the `.` and then the base expression span just before it.
    let dot = match find_dot(&chars, prefix_start) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let base_end = dot; // base ends at the dot
    let base_start = base_expr_start(&chars, base_end);

    // The checker keys types by occurrence span; look for the type recorded on a
    // span ending exactly at the base's end (the whole base expression).
    let base_ty = typed
        .types
        .iter()
        .filter(|((_, e), _)| *e == base_end)
        .max_by_key(|((s, _), _)| *s) // narrowest base wins
        .map(|(_, &tid)| tid)
        .or_else(|| typed.type_at(k2_syntax::Span::new(base_start, base_end, 0, 0)));

    let tid = match base_ty {
        Some(t) => t,
        None => return Vec::new(),
    };

    members_of_type(typed, tid)
}

/// Enumerates the member completion items of a type, auto-dereferencing one
/// pointer layer (mirroring `synth_field`). Opaque/module/deferred bases yield
/// no candidates (so we never offer something false).
fn members_of_type(typed: &k2_types::Typed, tid: k2_types::TypeId) -> Vec<JsonValue> {
    let arena = &typed.arena;
    let ty = arena.get(tid).clone();
    let ty = match ty {
        Type::Pointer { pointee, .. } => arena.get(pointee).clone(),
        other => other,
    };
    let mut items = Vec::new();
    match ty {
        Type::Struct(id) => {
            let info = &arena.structs[id.0 as usize];
            for f in &info.fields {
                items.push(item(&f.name, KIND_FIELD, Some(arena.fmt(f.ty))));
            }
            for d in &info.decls {
                items.push(item(&d.name, decl_kind(arena, d.ty), Some(arena.fmt(d.ty))));
            }
        }
        Type::Enum(id) => {
            let info = &arena.enums[id.0 as usize];
            for v in &info.variants {
                items.push(item(&v.name, KIND_ENUM_MEMBER, None));
            }
            for d in &info.decls {
                items.push(item(&d.name, decl_kind(arena, d.ty), Some(arena.fmt(d.ty))));
            }
        }
        Type::Union(id) => {
            let info = &arena.unions[id.0 as usize];
            for v in &info.variants {
                items.push(item(&v.name, KIND_FIELD, Some(arena.fmt(v.payload))));
            }
            for d in &info.decls {
                items.push(item(&d.name, decl_kind(arena, d.ty), Some(arena.fmt(d.ty))));
            }
        }
        Type::Slice { .. } | Type::Array { .. } => {
            // Built-in members of slices/arrays.
            items.push(item("len", KIND_FIELD, Some("usize".to_string())));
            items.push(item("ptr", KIND_FIELD, None));
        }
        // Module / opaque / deferred / anytype: no concrete members to offer.
        _ => {}
    }
    items
}

/// A member declaration is a method when its type is a function, else a field.
fn decl_kind(arena: &k2_types::TypeArena, ty: k2_types::TypeId) -> i64 {
    match arena.get(ty) {
        Type::Fn(_) => KIND_METHOD,
        _ => KIND_FIELD,
    }
}

/// Finds the `.` at or just before `prefix_start` (skipping intervening spaces).
fn find_dot(chars: &[char], prefix_start: u32) -> Option<u32> {
    let mut i = prefix_start as usize;
    while i > 0 && (chars[i - 1] == ' ' || chars[i - 1] == '\t') {
        i -= 1;
    }
    if i > 0 && chars[i - 1] == '.' {
        Some((i - 1) as u32)
    } else {
        None
    }
}

/// Walks left from `end` over a `base` expression — a run of identifier chars,
/// `.`-separated members, and bracketed/parenthesized groups — to find where the
/// base begins. Good enough to type the common `name`, `a.b`, and `f(x)` bases.
fn base_expr_start(chars: &[char], end: u32) -> u32 {
    let mut i = end as usize;
    while i > 0 {
        let c = chars[i - 1];
        if is_ident_char(c) || c == '.' {
            i -= 1;
        } else {
            break;
        }
    }
    i as u32
}

// =========================================================================
//  Shared item builder
// =========================================================================

/// Builds one `CompletionItem` JSON object.
fn item(label: &str, kind: i64, detail: Option<String>) -> JsonValue {
    let mut pairs = vec![
        ("label", JsonValue::str(label)),
        ("kind", JsonValue::num(kind)),
    ];
    if let Some(d) = detail {
        pairs.push(("detail", JsonValue::str(d)));
    }
    JsonValue::obj(pairs)
}
