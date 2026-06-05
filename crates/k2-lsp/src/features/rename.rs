//! `textDocument/rename` (+ `textDocument/prepareRename`): rename a binding at
//! every one of its occurrences.
//!
//! Rename reuses the same reverse-Uses primitive as references
//! ([`occurrences_of_def`](crate::features::occurrences_of_def)), but **always**
//! includes the declaration site — a rename that left the binding name untouched
//! would not compile. Local, parameter, and item renames are uniform: each is a
//! [`DefId`](k2_resolve::DefId) whose occurrences live in the Uses table plus the
//! `Def` span, so "rename a local vs a param vs an item, all correct" falls out of
//! the single shared collector.
//!
//! Validation rejects an invalid new identifier (non-ident or a reserved keyword)
//! so the editor surfaces a clear error instead of producing an un-lexable buffer.
//! A cursor that is not on a renameable binding (whitespace, a predeclared name, a
//! member, an unresolved name) yields a `null` result, not an error.
//!
//! A struct/union **field** or enum **tag** is deliberately *not* renameable: its
//! uses are member accesses (`p.x`, `Color.red`) recorded as
//! [`Resolution::DeferredMember`] and resolved by the type checker, so name
//! resolution cannot enumerate them. Offering a rename there would edit only the
//! declaration and silently corrupt every use, so — like a predeclared name —
//! `prepareRename` returns `null` and `rename` reports no target.

use k2_resolve::{DefKind, Resolution};

use crate::analysis::Analysis;
use crate::features::{
    def_at_offset_in, def_name_span, is_valid_ident, occurrences_of_def_in, use_at_offset,
};
use crate::json::JsonValue;

/// The outcome of a rename request, distinguishing "nothing to rename" (a `null`
/// result) from "the new name is invalid" (a JSON-RPC error).
pub enum RenameOutcome {
    /// A computed `WorkspaceEdit`.
    Edit(JsonValue),
    /// The cursor is not on a renameable symbol → return a `null` result.
    NoTarget,
    /// The proposed name is not a valid identifier → return an InvalidParams error.
    InvalidName,
}

/// Computes the rename `WorkspaceEdit` for the symbol at `offset`, renaming it to
/// `new_name` in document `uri`.
pub fn compute(analysis: &Analysis, uri: &str, offset: u32, new_name: &str) -> RenameOutcome {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return RenameOutcome::NoTarget,
    };

    // Resolve the cursor to a renameable binding. A predeclared/member/unresolved
    // position is *not* renameable; we report no target rather than an error.
    let id = match renameable_def_id(analysis, offset) {
        Some(id) => id,
        None => return RenameOutcome::NoTarget,
    };

    // Validate the new name only once we know there is something to rename, so a
    // stray cursor never produces a spurious "invalid name" error.
    if !is_valid_ident(new_name) {
        return RenameOutcome::InvalidName;
    }

    // Edits, sorted descending by start so a client applying them in order never
    // shifts a not-yet-applied edit's offsets (defensive; edits are non-overlapping).
    let mut spans = occurrences_of_def_in(resolved, id, Some(&analysis.source));
    spans.sort_by_key(|s| std::cmp::Reverse((s.start, s.end)));
    let edits: Vec<JsonValue> = spans
        .into_iter()
        .map(|span| {
            JsonValue::obj(vec![
                ("range", analysis.posmap.span_to_range(span)),
                ("newText", JsonValue::str(new_name)),
            ])
        })
        .collect();

    if edits.is_empty() {
        return RenameOutcome::NoTarget;
    }

    let changes = JsonValue::Object(vec![(uri.to_string(), JsonValue::arr(edits))]);
    let workspace_edit = JsonValue::obj(vec![("changes", changes)]);
    RenameOutcome::Edit(workspace_edit)
}

/// Computes the `prepareRename` response at `offset`: the range of the identifier
/// under the cursor plus its current name, or `null` when not on a renameable
/// symbol.
pub fn prepare(analysis: &Analysis, offset: u32) -> JsonValue {
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return JsonValue::Null,
    };

    // A renameable use (Def/Module, never Predeclared/member/unresolved) keyed by
    // its own occurrence span, or a declaration site the cursor rests on.
    if let Some(use_) = use_at_offset(resolved, offset) {
        match use_.res {
            Resolution::Def(_) | Resolution::Module(_) => {
                return JsonValue::obj(vec![
                    ("range", analysis.posmap.span_to_range(use_.span)),
                    ("placeholder", JsonValue::str(use_.name.clone())),
                ]);
            }
            // A predeclared name (`i32`) is not renameable; nor is a member or an
            // unresolved name.
            Resolution::Predeclared(_) | Resolution::DeferredMember | Resolution::Error => {
                return JsonValue::Null
            }
        }
    }
    if let Some(def) = def_at_offset_in(resolved, offset, &analysis.source) {
        // A struct/union field or enum tag declaration is *not* renameable: its
        // uses are member accesses (`p.x`, `Color.red`), recorded as
        // `Resolution::DeferredMember` and resolved by the type checker — name
        // resolution never maps them back to this `DefId`, so a rename would edit
        // only the declaration and silently corrupt every use. Mirror the
        // Predeclared exclusion and report "cannot rename here" (a null result)
        // rather than offer a partial, code-breaking edit.
        if def.kind == DefKind::Field {
            return JsonValue::Null;
        }
        let name_span = def_name_span(def, &analysis.source);
        return JsonValue::obj(vec![
            ("range", analysis.posmap.span_to_range(name_span)),
            ("placeholder", JsonValue::str(def.name.clone())),
        ]);
    }
    JsonValue::Null
}

/// The renameable [`DefId`](k2_resolve::DefId) at `offset`, treating a predeclared
/// name *and* a struct/union field or enum tag as *not* renameable (so a cursor on
/// `i32`, or on a field/tag declaration whose `p.x`/`Color.red` uses are
/// `DeferredMember`, produces no edit instead of a partial, corrupting one).
fn renameable_def_id(analysis: &Analysis, offset: u32) -> Option<k2_resolve::DefId> {
    let resolved = analysis.resolved.as_ref()?;
    if let Some(use_) = use_at_offset(resolved, offset) {
        match use_.res {
            Resolution::Def(id) | Resolution::Module(id) => return Some(id),
            // Predeclared names are not user bindings; member/error carry no DefId.
            Resolution::Predeclared(_) | Resolution::DeferredMember | Resolution::Error => {}
        }
    }
    let def = def_at_offset_in(resolved, offset, &analysis.source)?;
    // A field/enum-tag declaration carries a real `DefId`, but its uses are
    // `DeferredMember` and are never recorded against it — renaming would touch
    // only the declaration. Exclude it so `compute` reports `NoTarget`.
    if def.kind == DefKind::Field {
        return None;
    }
    Some(def.id)
}
