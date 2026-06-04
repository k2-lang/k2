//! `textDocument/definition`: go-to-definition via the resolver's Uses → Def
//! map, with member positions resolved through the type checker's member table.
//!
//! In the single-buffer model every definition lives in the same document, so a
//! `Location` reuses the request's own URI. A predeclared name (`i32`) has a
//! synthetic zero-width span at offset 0 and is *not* a real source location, so
//! we return `null` rather than jumping to the top of the file.

use k2_resolve::{DefKind, Resolution};
use k2_syntax::Span;
use k2_types::{MemberRes, Type};

use crate::analysis::Analysis;
use crate::features::use_at_offset;
use crate::json::JsonValue;

/// Computes the definition `Location` at scalar `offset` for document `uri`, or
/// `JsonValue::Null`.
pub fn compute(analysis: &Analysis, uri: &str, offset: u32) -> JsonValue {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::Null,
    };
    let use_ = match use_at_offset(resolved, offset) {
        Some(u) => u,
        None => return JsonValue::Null,
    };

    let target: Option<Span> = match use_.res {
        Resolution::Def(id) | Resolution::Predeclared(id) | Resolution::Module(id) => {
            let def = &resolved.defs[id.index()];
            // Predeclared names have a synthetic zero-width span; skip them.
            if def.kind == DefKind::Predeclared {
                None
            } else {
                Some(def.span)
            }
        }
        Resolution::DeferredMember => member_def_span(analysis, use_.span),
        Resolution::Error => None,
    };

    match target {
        Some(span) if span.end > span.start || span.start > 0 => location(analysis, uri, span),
        _ => JsonValue::Null,
    }
}

/// Resolves a member occurrence (keyed by the whole `base.field` access span) to
/// its field/variant/decl span.
///
/// The checker records the *index* of the field/variant, but not the owning
/// aggregate, so we recover the base type from the type recorded for the base
/// sub-expression (everything before the last `.` of the access) and index into
/// that specific aggregate. This is exact, not a name-collision guess.
fn member_def_span(analysis: &Analysis, member_span: Span) -> Option<Span> {
    let typed = analysis.typed.as_ref()?;
    let resolved = analysis.resolved.as_ref()?;
    let member = typed.members.get(&(member_span.start, member_span.end))?;
    match *member {
        MemberRes::Decl(def_id) => Some(resolved.defs[def_id.index()].span),
        MemberRes::Field(idx) => {
            let agg = base_aggregate(analysis, member_span)?;
            field_span(typed, agg, idx)
        }
        MemberRes::Variant(idx) => {
            let agg = base_aggregate(analysis, member_span)?;
            variant_span(typed, agg, idx)
        }
        MemberRes::BuiltinField | MemberRes::ErrorMember | MemberRes::Deferred => None,
    }
}

/// The aggregate type of the *base* of a member access, found from the type
/// recorded on the base sub-expression (the part before the last `.`), with one
/// pointer auto-deref applied (mirroring `synth_field`).
fn base_aggregate(analysis: &Analysis, member_span: Span) -> Option<Type> {
    let typed = analysis.typed.as_ref()?;
    let chars: Vec<char> = analysis.source.chars().collect();
    // The last '.' within the access span separates base from field.
    let dot = (member_span.start..member_span.end)
        .rev()
        .find(|&i| chars.get(i as usize) == Some(&'.'))?;
    // The base sub-expression occupies [member_span.start, dot).
    let base_ty = typed
        .types
        .iter()
        .filter(|((s, e), _)| *s == member_span.start && *e == dot)
        .map(|(_, &tid)| tid)
        .next()?;
    let ty = typed.arena.get(base_ty).clone();
    Some(match ty {
        Type::Pointer { pointee, .. } => typed.arena.get(pointee).clone(),
        other => other,
    })
}

/// The span of field `idx` of an aggregate type, if it is a struct/union.
fn field_span(typed: &k2_types::Typed, ty: Type, idx: u32) -> Option<Span> {
    match ty {
        Type::Struct(id) => typed.arena.structs[id.0 as usize]
            .fields
            .get(idx as usize)
            .map(|f| f.span),
        Type::Union(id) => typed.arena.unions[id.0 as usize]
            .variants
            .get(idx as usize)
            .map(|v| v.span),
        _ => None,
    }
}

/// The span of variant `idx` of an enum/union aggregate type.
fn variant_span(typed: &k2_types::Typed, ty: Type, idx: u32) -> Option<Span> {
    match ty {
        Type::Enum(id) => typed.arena.enums[id.0 as usize]
            .variants
            .get(idx as usize)
            .map(|v| v.span),
        Type::Union(id) => typed.arena.unions[id.0 as usize]
            .variants
            .get(idx as usize)
            .map(|v| v.span),
        _ => None,
    }
}

/// Builds an LSP `Location { uri, range }` for a span in the open document.
fn location(analysis: &Analysis, uri: &str, span: Span) -> JsonValue {
    JsonValue::obj(vec![
        ("uri", JsonValue::str(uri)),
        ("range", analysis.posmap.span_to_range(span)),
    ])
}
