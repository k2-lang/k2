//! The per-document analysis bundle: one parse + resolve + check pass over a
//! single open buffer, plus the position map for coordinate conversion.
//!
//! ## Why raw `parse`, not the driver's `parse_program`
//!
//! The `k2c` driver's `parse_program` appends the entire standard-library source
//! after the user's text so `@import("std")` resolves to a real compiled module.
//! That is right for *compiling*, but wrong for an interactive single buffer:
//! the appended std items would pollute scope-aware completion and the user's
//! spans would coexist with std spans in the same map. For the editor we want
//! resolution/types over **exactly** the user's source, so spans map 1:1 to the
//! editor buffer. The resolver and type layer are designed to *not* error on
//! opaque module member chains (`std.heap.X` resolves as `DeferredMember` /
//! `Type::Deferred`), so hover/definition over std members degrade gracefully to
//! "no info" rather than producing false diagnostics.
//!
//! ## Staged gating mirrors `cmd_check`
//!
//! Resolution runs only on a clean parse and type-checking only on a clean
//! resolve, exactly like the driver. But diagnostics from *every* stage that ran
//! are still published — a parse or resolve error is precisely what the user
//! needs to see.

use k2_parse::{parse, ParseResult};
use k2_resolve::{resolve_file, Resolved};
use k2_types::{check_file, Typed};

use crate::position::PositionMap;

/// A fully computed analysis of one document.
pub struct Analysis {
    /// The exact source the analysis was computed from.
    pub source: String,
    /// The LSP/scalar position map over `source`.
    pub posmap: PositionMap,
    /// The parse result (AST + diagnostics); always present.
    pub parse: ParseResult,
    /// The resolved side-table, if the parse had no errors.
    pub resolved: Option<Resolved>,
    /// The typed side-table, if resolution had no errors.
    pub typed: Option<Typed>,
}

impl Analysis {
    /// Runs the front-end pipeline over `source`, gating each stage on the
    /// previous one being error-free (mirroring the driver).
    pub fn compute(source: String) -> Analysis {
        let posmap = PositionMap::new(&source);
        let parse = parse(&source);

        let resolved = if parse.is_ok() {
            Some(resolve_file(&parse.file))
        } else {
            None
        };

        let typed = match &resolved {
            Some(r) if r.is_ok() => Some(check_file(&parse.file, r)),
            _ => None,
        };

        Analysis {
            source,
            posmap,
            parse,
            resolved,
            typed,
        }
    }
}
