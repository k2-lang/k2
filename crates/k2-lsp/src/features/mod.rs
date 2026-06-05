//! The LSP feature providers, each reusing the front-end crates with no
//! re-implementation of compiler logic.
//!
//! Every provider takes an [`Analysis`](crate::analysis::Analysis) and the LSP
//! request position, converts the position to a scalar offset with the
//! [`PositionMap`](crate::position::PositionMap), and answers from the
//! resolver/type-checker side tables.

pub mod code_action;
pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod formatting;
pub mod hover;
pub mod inlay_hint;
pub mod references;
pub mod rename;
pub mod semantic_tokens;
pub mod signature_help;

use k2_lexer::keyword_kind;
use k2_resolve::{Def, DefId, DefKind, Resolution, Resolved, Use};
use k2_syntax::Span;

/// Finds the identifier occurrence (a resolver [`Use`]) covering scalar `offset`,
/// preferring the narrowest match so a nested identifier wins over an enclosing
/// chain. Returns `None` when the cursor is not on any recorded occurrence.
///
/// The upper bound is **inclusive** (`offset <= span.end`), so a cursor resting
/// just past the last character of an identifier — the ubiquitous "caret at end
/// of word" position — still resolves. rust-analyzer/clangd/gopls use the same
/// inclusive end. The narrowest-span tie-break keeps the innermost identifier
/// winning when two adjacent occurrences share a boundary offset.
pub fn use_at_offset(resolved: &Resolved, offset: u32) -> Option<&Use> {
    resolved
        .uses
        .list
        .iter()
        .filter(|u| u.span.start <= offset && offset <= u.span.end)
        .min_by_key(|u| u.span.end - u.span.start)
}

/// Finds the *declaration* (a resolver [`Def`]) whose **name token** covers scalar
/// `offset`, preferring the narrowest enclosing declaration. Returns `None` when
/// the cursor is not resting on a binding name.
///
/// Where [`use_at_offset`] answers "the cursor is on a *reference*", this answers
/// "the cursor is on the *binding name* itself" — needed by references/rename so a
/// query on a declaration (not just a use) still works. The match is on the name
/// token (via [`def_name_span`]) rather than the whole declaration span, so a
/// cursor on the `const` keyword or the type annotation does **not** spuriously
/// resolve to the binding. Predeclared names (synthetic zero-width spans) are
/// skipped, requiring the source to recover the name token.
pub fn def_at_offset_in<'a>(resolved: &'a Resolved, offset: u32, source: &str) -> Option<&'a Def> {
    resolved
        .defs
        .iter()
        .filter(|d| d.kind != DefKind::Predeclared)
        .filter(|d| d.span.end > d.span.start)
        .filter(|d| {
            let name = def_name_span(d, source);
            name.start <= offset && offset <= name.end
        })
        .min_by_key(|d| d.span.end - d.span.start)
}

/// As [`def_at_offset_in`] but without the source: matches on the whole
/// declaration span (a coarser test used only where the source is unavailable).
pub fn def_at_offset(resolved: &Resolved, offset: u32) -> Option<&Def> {
    resolved
        .defs
        .iter()
        .filter(|d| d.kind != DefKind::Predeclared)
        .filter(|d| d.span.end > d.span.start)
        .filter(|d| d.span.start <= offset && offset <= d.span.end)
        .min_by_key(|d| d.span.end - d.span.start)
}

/// Recovers the canonical [`DefId`] the cursor at `offset` refers to, whether the
/// cursor sits on a *use* of a binding or on the *declaration name* itself.
///
/// A `Use` wins first (the common case — the cursor is on a reference); failing
/// that, a `Def` whose **name token** covers the offset is used (the cursor is on
/// the binding name, not the keyword or annotation). `DeferredMember`/`Error`
/// resolutions carry no `DefId` and yield `None`, so member positions and
/// unresolved names are not treated as renameable bindings.
pub fn def_id_at_offset(resolved: &Resolved, offset: u32) -> Option<DefId> {
    def_id_at_offset_in(resolved, offset, None)
}

/// As [`def_id_at_offset`], using `source` (when provided) to match a declaration
/// on its precise name token rather than the whole declaration span.
pub fn def_id_at_offset_in(
    resolved: &Resolved,
    offset: u32,
    source: Option<&str>,
) -> Option<DefId> {
    if let Some(use_) = use_at_offset(resolved, offset) {
        match use_.res {
            Resolution::Def(id) | Resolution::Predeclared(id) | Resolution::Module(id) => {
                return Some(id);
            }
            Resolution::DeferredMember | Resolution::Error => {}
        }
    }
    match source {
        Some(src) => def_at_offset_in(resolved, offset, src).map(|d| d.id),
        None => def_at_offset(resolved, offset).map(|d| d.id),
    }
}

/// Collects every occurrence of the binding `id`: its declaration *name token*
/// (recovered from within the `Def` span) plus every `Use` whose resolution names
/// the same `DefId`. The result is sorted by `(start, end)` and deduplicated, so
/// references/rename get one entry per distinct source position in a deterministic
/// order.
///
/// This is the *reverse* of the resolver's Uses map (occurrence → `Def`):
/// `occurrences_of_def` gathers, for a fixed `Def`, all the occurrences that map
/// to it. Member/`DeferredMember` uses carry no `DefId` and are therefore never
/// included — they are resolved through the type checker, not name resolution.
///
/// The declaration occurrence is the **name token**, not the whole declaration:
/// the resolver records an item's/local's `Def.span` as the entire declaration
/// (`const x: i32 = 1`), so a rename that replaced `Def.span` would clobber the
/// whole statement. [`def_name_span`] narrows it to just the bound identifier.
pub fn occurrences_of_def(resolved: &Resolved, id: DefId) -> Vec<Span> {
    occurrences_of_def_in(resolved, id, None)
}

/// As [`occurrences_of_def`], but `source` (when provided) lets the declaration
/// occurrence be narrowed to the exact name token via [`def_name_span`]. Callers
/// that have the source pass `Some(source)`; the source-free path falls back to
/// the raw `Def.span`.
pub fn occurrences_of_def_in(resolved: &Resolved, id: DefId, source: Option<&str>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    // The declaration site, narrowed to the name token, unless synthetic.
    let def = &resolved.defs[id.index()];
    if def.kind != DefKind::Predeclared && def.span.end > def.span.start {
        let decl = match source {
            Some(src) => def_name_span(def, src),
            None => def.span,
        };
        spans.push(decl);
    }
    // Every reference that resolved to this same binding.
    for use_ in &resolved.uses.list {
        let matches = match use_.res {
            Resolution::Def(uid) | Resolution::Predeclared(uid) | Resolution::Module(uid) => {
                uid == id
            }
            Resolution::DeferredMember | Resolution::Error => false,
        };
        if matches {
            spans.push(use_.span);
        }
    }
    spans.sort_by_key(|s| (s.start, s.end));
    spans.dedup_by_key(|s| (s.start, s.end));
    spans
}

/// Narrows a declaration `Def`'s span (which the resolver records as the *whole*
/// declaration, e.g. `const x: i32 = 1`) to just the bound name token.
///
/// The name is always the first whole-word occurrence of `def.name` inside the
/// declaration span (it immediately follows the `const`/`var`/`pub fn` keyword
/// prefix, before any initializer that might mention the name again). For a
/// parameter/capture the span already starts at the name, so this finds it at the
/// span start. Falls back to the whole span if the name is not found verbatim.
pub fn def_name_span(def: &Def, source: &str) -> Span {
    let chars: Vec<char> = source.chars().collect();
    let name: Vec<char> = def.name.chars().collect();
    if name.is_empty() {
        return def.span;
    }
    let lo = def.span.start as usize;
    let hi = (def.span.end as usize).min(chars.len());
    let n = name.len();
    let mut i = lo;
    while i + n <= hi {
        if chars[i..i + n] == name[..] {
            let before_ok = i == 0 || !is_ident_char(chars[i - 1]);
            let after_ok = i + n >= chars.len() || !is_ident_char(chars[i + n]);
            if before_ok && after_ok {
                return Span::new(i as u32, (i + n) as u32, def.span.line, def.span.col);
            }
        }
        i += 1;
    }
    def.span
}

/// `true` if `c` may appear within a k2 identifier.
fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// `true` if `name` is a valid k2 identifier for a *rename* target: non-empty,
/// first character `_`/ASCII-alpha, the rest `_`/ASCII-alphanumeric, and not a
/// reserved keyword (so a rename to `fn`/`const` is rejected).
///
/// This mirrors the lexer's identifier rule and its keyword set exactly, so a
/// rename can never produce a buffer the lexer would re-tokenize differently than
/// the editor intends.
pub fn is_valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    if !chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return false;
    }
    keyword_kind(name).is_none()
}
