//! The LSP feature providers, each reusing the front-end crates with no
//! re-implementation of compiler logic.
//!
//! Every provider takes an [`Analysis`](crate::analysis::Analysis) and the LSP
//! request position, converts the position to a scalar offset with the
//! [`PositionMap`](crate::position::PositionMap), and answers from the
//! resolver/type-checker side tables.

pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod formatting;
pub mod hover;

use k2_resolve::{Resolved, Use};

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
