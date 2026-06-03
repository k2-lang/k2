//! Deterministic, greppable dumps of a [`Typed`] result.
//!
//! [`dump_signatures`] renders one line per typed definition (a function's
//! signature, a const's type, …), in definition order, for the `k2c check`
//! summary and golden tests. [`dump_types`] additionally lists every recorded
//! occurrence type, keyed by span — the answer to "what type does the expression
//! at this position have?".

use std::fmt::Write as _;

use k2_resolve::{DefKind, Resolved};

use crate::Typed;

/// Renders one line per typed value/fn definition: ``def#N `name` : Type``, in
/// the resolver's definition order (deterministic). Predeclared definitions are
/// skipped. The list is non-empty for any file with at least one declaration.
pub fn dump_signatures(typed: &Typed, resolved: &Resolved) -> String {
    let mut out = String::new();
    for def in &resolved.defs {
        if matches!(def.kind, DefKind::Predeclared) {
            continue;
        }
        // Only the top-level binding kinds carry a signature worth showing.
        if !matches!(
            def.kind,
            DefKind::Item | DefKind::Param | DefKind::Local | DefKind::Field | DefKind::Module
        ) {
            continue;
        }
        if matches!(def.kind, DefKind::Module) {
            let _ = writeln!(out, "def#{} module `{}`", def.id.0, def.name);
        } else if let Some(&ty) = typed.binding_types.get(&def.id) {
            let _ = writeln!(
                out,
                "def#{} {} `{}` : {}",
                def.id.0,
                kind_word(def.kind),
                def.name,
                typed.arena.fmt(ty)
            );
        }
    }
    out
}

/// Renders the signatures, then every recorded occurrence type, keyed by span.
pub fn dump_types(typed: &Typed, resolved: &Resolved) -> String {
    let mut out = dump_signatures(typed, resolved);
    let _ = writeln!(out, "(types");
    // Sort occurrences by (start, end) for a stable dump.
    let mut entries: Vec<(&(u32, u32), &crate::TypeId)> = typed.types.iter().collect();
    entries.sort_by_key(|(k, _)| **k);
    for ((start, end), ty) in entries {
        let _ = writeln!(out, "  @{start}..{end} : {}", typed.arena.fmt(*ty));
    }
    let _ = writeln!(out, ")");
    out
}

/// A short word for a definition kind, for the dump.
fn kind_word(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Item => "item",
        DefKind::Param => "param",
        DefKind::Local => "local",
        DefKind::Field => "field",
        DefKind::Capture => "capture",
        DefKind::Module => "module",
        DefKind::Predeclared => "predeclared",
    }
}
