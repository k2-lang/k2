//! Deterministic, greppable pretty-dumps of a [`Resolved`] side-table.
//!
//! The format mirrors the parser's S-expression style: nested, parenthesized,
//! indented two spaces per level, and stable across runs. It exists for tooling
//! and golden tests — "what does the name at this position mean, and what is the
//! shape of the scope tree?" Two entry points are provided: [`dump_scopes`]
//! renders the scope/definition tree, and [`dump_resolution`] additionally lists
//! every identifier use and the module graph.

use crate::def::DefKind;
use crate::ids::ScopeId;
use crate::module::ModuleRef;
use crate::resolver::Resolved;
use crate::uses::Resolution;
use std::fmt::Write as _;

/// Renders the scope tree (with the definitions declared in each scope) as a
/// nested S-expression. The predeclared root is summarized rather than expanded
/// to keep the output focused on user code.
pub fn dump_scopes(r: &Resolved) -> String {
    let mut out = String::new();
    // The predeclared root is summarized (it is large and fixed).
    let _ = writeln!(
        out,
        "(scope #0 predeclared {} names)",
        r.scopes[0].names.len()
    );
    write_scope(&mut out, r, r.file_scope, 0);
    out
}

/// Renders the scope tree, then the full uses table and module graph. This is
/// the richest dump, used by `k2c resolve --uses`.
pub fn dump_resolution(r: &Resolved) -> String {
    let mut out = dump_scopes(r);
    let _ = writeln!(out, "(uses");
    for u in &r.uses.list {
        let desc = describe_resolution(r, u.res);
        let name = if u.name.is_empty() { "·" } else { &u.name };
        let _ = writeln!(
            out,
            "  `{}` @{}..{} -> {}",
            name, u.span.start, u.span.end, desc
        );
    }
    let _ = writeln!(out, ")");
    let _ = writeln!(out, "(modules");
    for m in &r.modules {
        let desc = match &m.reference {
            ModuleRef::WellKnown(n) => format!("well-known {n}"),
            ModuleRef::Path(p) => format!("path {}", p.display()),
            ModuleRef::Unresolved => "unresolved".to_string(),
        };
        let _ = writeln!(out, "  #{} -> {}", m.id.0, desc);
    }
    let _ = writeln!(out, ")");
    out
}

/// Recursively writes `scope` and all of its child scopes at `depth`.
fn write_scope(out: &mut String, r: &Resolved, scope: ScopeId, depth: usize) {
    let s = &r.scopes[scope.index()];
    let indent = "  ".repeat(depth + 1);
    let _ = writeln!(out, "{indent}(scope #{} {}", s.id.0, kind_word(s.kind));

    let child_indent = "  ".repeat(depth + 2);
    // Definitions declared directly in this scope.
    for (_name, id) in &s.names {
        let d = &r.defs[id.index()];
        let extra = match d.kind {
            DefKind::Module => {
                let m = d.module.map(|mid| module_word(r, mid)).unwrap_or_default();
                format!(" -> {m}")
            }
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "{child_indent}(def #{} {} `{}` @{}..{}{})",
            d.id.0,
            kind_def_word(d.kind),
            d.name,
            d.span.start,
            d.span.end,
            extra
        );
    }
    // Child scopes, in id order (which is creation = source order).
    let children: Vec<ScopeId> = r
        .scopes
        .iter()
        .filter(|c| c.parent == Some(scope))
        .map(|c| c.id)
        .collect();
    for child in children {
        write_scope(out, r, child, depth + 1);
    }

    let _ = writeln!(out, "{indent})");
}

/// A short word for a scope kind.
fn kind_word(kind: crate::scope::ScopeKind) -> &'static str {
    use crate::scope::ScopeKind::*;
    match kind {
        Predeclared => "predeclared",
        File => "file",
        Container => "container",
        Params => "params",
        Block => "block",
        Capture => "capture",
    }
}

/// A short word for a definition kind.
fn kind_def_word(kind: DefKind) -> &'static str {
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

/// A short description of a module node, e.g. `module std`.
fn module_word(r: &Resolved, mid: crate::ids::ModuleId) -> String {
    match &r.modules[mid.index()].reference {
        ModuleRef::WellKnown(n) => format!("module {n}"),
        ModuleRef::Path(p) => format!("module path:{}", p.display()),
        ModuleRef::Unresolved => "module unresolved".to_string(),
    }
}

/// A short description of a resolution, naming the target definition kind.
fn describe_resolution(r: &Resolved, res: Resolution) -> String {
    match res {
        Resolution::Def(id) => {
            format!("def#{}({})", id.0, kind_def_word(r.defs[id.index()].kind))
        }
        Resolution::Predeclared(id) => format!("predeclared#{}", id.0),
        Resolution::Module(id) => format!("module#{}", id.0),
        Resolution::DeferredMember => "deferred-member".to_string(),
        Resolution::Error => "error".to_string(),
    }
}
