//! `textDocument/hover`: the type and definition-kind of the symbol under the
//! cursor.
//!
//! The type comes from the type checker's per-occurrence map (`Typed::type_at`),
//! rendered with `TypeArena::fmt`; the kind/name comes from the resolver's `Use`
//! → `Def`. When the occurrence is a member position the type comes from the
//! member's binding type, and the member *name* and highlight range are recovered
//! from the source (the resolver records member uses with an empty name and the
//! whole `base.member` span). Nothing under the cursor yields a `null` result.

use k2_resolve::{DefKind, Resolution};

use crate::analysis::Analysis;
use crate::features::use_at_offset;
use crate::json::JsonValue;

/// Computes the hover result at scalar `offset`, or `JsonValue::Null`.
pub fn compute(analysis: &Analysis, offset: u32) -> JsonValue {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::Null,
    };
    let use_ = match use_at_offset(resolved, offset) {
        Some(u) => u,
        None => return JsonValue::Null,
    };

    // The kind label and (when known) the name, from the resolution. A member
    // access is recorded by the resolver with an empty name and the whole
    // `base.member` span, so for it we recover the member identifier — and the
    // tighter range over just that identifier — from the source.
    let (kind_label, name) = describe(resolved, use_.res, &use_.name);
    let (name, range_span) = match use_.res {
        Resolution::DeferredMember => match member_ident(analysis, use_.span) {
            Some((member_name, span)) => (member_name, span),
            None => (name, use_.span),
        },
        _ => (name, use_.span),
    };

    // The type string, if the checker recorded one at this occurrence or at the
    // referenced binding.
    let type_str = type_of(analysis, use_);

    let value = match &type_str {
        Some(t) => format!("```k2\n{}: {}\n```\n{}", name, t, kind_label),
        None => format!("```k2\n{}\n```\n{}", name, kind_label),
    };

    JsonValue::obj(vec![
        (
            "contents",
            JsonValue::obj(vec![
                ("kind", JsonValue::str("markdown")),
                ("value", JsonValue::str(value)),
            ]),
        ),
        ("range", analysis.posmap.span_to_range(range_span)),
    ])
}

/// Recovers the member identifier of a `base.member` access from the source.
///
/// A `DeferredMember` use carries an empty name and the whole access span (`p.xx`,
/// `a.b.c`, `.Red`, `error.Name`), so the displayed name and the highlight range
/// are derived here: the text after the last `.` within the span is the member,
/// and the range is `[last_dot + 1, span.end)` so only the member token is
/// highlighted, not the base and dot. Returns `None` when there is no `.` in the
/// span (a degenerate occurrence), letting the caller fall back to the raw use.
fn member_ident(analysis: &Analysis, span: k2_syntax::Span) -> Option<(String, k2_syntax::Span)> {
    let chars: Vec<char> = analysis.source.chars().collect();
    let dot = (span.start..span.end)
        .rev()
        .find(|&i| chars.get(i as usize) == Some(&'.'))?;
    let start = dot + 1;
    let name: String = chars[start as usize..span.end as usize].iter().collect();
    let member_span = k2_syntax::Span::new(start, span.end, span.line, span.col);
    Some((name, member_span))
}

/// Describes a resolution as a `(kind_label, name)` pair.
fn describe(
    resolved: &k2_resolve::Resolved,
    res: Resolution,
    fallback_name: &str,
) -> (String, String) {
    match res {
        Resolution::Def(id) | Resolution::Predeclared(id) | Resolution::Module(id) => {
            let def = &resolved.defs[id.index()];
            (kind_label(def.kind).to_string(), def.name.clone())
        }
        Resolution::DeferredMember => ("member".to_string(), fallback_name.to_string()),
        Resolution::Error => ("unresolved".to_string(), fallback_name.to_string()),
    }
}

/// The human-readable label for a definition kind.
fn kind_label(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Item => "item",
        DefKind::Param => "parameter",
        DefKind::Local => "local",
        DefKind::Field => "field",
        DefKind::Capture => "capture",
        DefKind::Module => "module",
        DefKind::Predeclared => "builtin",
    }
}

/// The rendered type for an occurrence, preferring the per-occurrence type, then
/// the referenced binding's type.
fn type_of(analysis: &Analysis, use_: &k2_resolve::Use) -> Option<String> {
    let typed = analysis.typed.as_ref()?;
    // Per-occurrence type recorded at this span.
    if let Some(tid) = typed.type_at(use_.span) {
        return Some(typed.arena.fmt(tid));
    }
    // Otherwise the type bound to the referenced definition.
    if let Resolution::Def(id) | Resolution::Predeclared(id) | Resolution::Module(id) = use_.res {
        if let Some(&tid) = typed.binding_types.get(&id) {
            return Some(typed.arena.fmt(tid));
        }
    }
    None
}
