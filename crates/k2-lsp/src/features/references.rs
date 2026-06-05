//! `textDocument/references`: every occurrence of the symbol under the cursor.
//!
//! This is the reverse of the resolver's Uses map. The cursor resolves to a
//! single [`DefId`](k2_resolve::DefId) — whether it rests on a *use* or on the
//! *declaration* — and [`occurrences_of_def`](crate::features::occurrences_of_def)
//! then gathers the declaration span plus every `Use` that maps back to that same
//! binding. Local vs parameter vs item all flow through one code path, because all
//! three carry a `DefId` and record their references identically.
//!
//! A struct/union **field** or enum **tag** is excluded: its uses are member
//! accesses (`p.x`, `Color.red`) recorded as `Resolution::DeferredMember`, which
//! name resolution never maps back to the field's `DefId`. Reporting only the
//! declaration would falsely present an incomplete result as complete, so — like
//! the matching rename gate — such a cursor yields an empty array.
//!
//! `includeDeclaration` is honored: when `false` the binding's own declaration
//! span is dropped, leaving only the references. The result is a `Location[]`
//! sorted by position; an empty array (never `null`) is returned when nothing
//! under the cursor names a renameable binding, so the editor shows a clean "no
//! references" rather than an error.

use k2_resolve::DefKind;

use crate::analysis::Analysis;
use crate::features::{def_id_at_offset_in, def_name_span, occurrences_of_def_in};
use crate::json::JsonValue;

/// If the symbol at `offset` is a top-level `pub` item (a const/var/fn at file
/// scope), returns its name — the key a cross-file references scan looks for as
/// `module.name` in other open documents. Returns `None` for a local, parameter,
/// non-`pub` item, member, or unresolved position (none of which are reachable
/// across a path import).
pub fn top_level_item_name(analysis: &Analysis, offset: u32) -> Option<String> {
    let resolved = analysis.resolved.as_ref()?;
    let id = def_id_at_offset_in(resolved, offset, Some(&analysis.source))?;
    let def = &resolved.defs[id.index()];
    // Only a `pub` file-scope item is importable across files.
    if def.kind == DefKind::Item && def.is_pub && def.scope == resolved.file_scope {
        Some(def.name.clone())
    } else {
        None
    }
}

/// Computes the `Location[]` of every occurrence of the symbol at `offset` in
/// document `uri`. `include_declaration` controls whether the binding's own
/// declaration is part of the result.
pub fn compute(
    analysis: &Analysis,
    uri: &str,
    offset: u32,
    include_declaration: bool,
) -> JsonValue {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::arr(Vec::new()),
    };
    let id = match def_id_at_offset_in(resolved, offset, Some(&analysis.source)) {
        Some(id) => id,
        None => return JsonValue::arr(Vec::new()),
    };

    // A struct/union field or enum tag's uses are member accesses recorded as
    // `Resolution::DeferredMember`; name resolution never maps them back to this
    // `DefId`, so the only occurrence we could report is the declaration itself.
    // Returning just that would falsely present an incomplete result as the
    // complete set of references, so — consistent with rename gating the same
    // bindings — we report no references rather than a misleading single hit.
    if resolved.defs[id.index()].kind == DefKind::Field {
        return JsonValue::arr(Vec::new());
    }

    let decl_span = {
        let def = &resolved.defs[id.index()];
        if def.kind != DefKind::Predeclared && def.span.end > def.span.start {
            let name = def_name_span(def, &analysis.source);
            Some((name.start, name.end))
        } else {
            None
        }
    };

    let locations: Vec<JsonValue> = occurrences_of_def_in(resolved, id, Some(&analysis.source))
        .into_iter()
        // Drop the declaration occurrence when the client opted out of it.
        .filter(|span| include_declaration || decl_span != Some((span.start, span.end)))
        .map(|span| {
            JsonValue::obj(vec![
                ("uri", JsonValue::str(uri)),
                ("range", analysis.posmap.span_to_range(span)),
            ])
        })
        .collect();

    JsonValue::arr(locations)
}
