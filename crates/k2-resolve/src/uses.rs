//! Uses: what every identifier *occurrence* resolved to.
//!
//! Where a [`Def`](crate::def::Def) is a binding *site*, a [`Use`] is a binding
//! *reference* — one identifier occurrence in the source and the [`Resolution`]
//! it was given. Uses are keyed by their occurrence [`Span`], which is unique
//! per identifier in a single file, so tooling can ask "what does the name at
//! this exact position mean?" in O(1).

use crate::ids::DefId;
use k2_syntax::Span;
use std::collections::HashMap;

/// What an identifier occurrence resolved to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// A user binding: a local, parameter, capture, or file/container item.
    Def(DefId),
    /// A language-predeclared name (primitive/capability/C-alias). The `DefId`
    /// points at the predeclared `Def`.
    Predeclared(DefId),
    /// The base of a member-access chain that resolved to an imported module
    /// namespace (`std` in `std.heap.X`). The `DefId` is the module's `Def`.
    Module(DefId),
    /// A position deliberately *not* resolved in v0.4 because it names a member
    /// of a not-yet-typed value: a field name, an enum literal (`.Name`), an
    /// error literal (`error.Name`), or an initializer field name. These are
    /// resolved during type-checking (v0.5). Recorded for tooling; never an
    /// error.
    DeferredMember,
    /// The name could not be resolved; a diagnostic was emitted for it.
    Error,
}

/// One identifier occurrence and its resolution.
#[derive(Clone, Debug)]
pub struct Use {
    /// The referenced name as written.
    pub name: String,
    /// The occurrence span — the key identifying this use.
    pub span: Span,
    /// What the name resolved to.
    pub res: Resolution,
}

/// The full table of identifier occurrences, kept in source order for
/// deterministic dumps, plus a `(start, end)` index for O(1) lookup by tooling.
#[derive(Default)]
pub struct Uses {
    /// Every recorded use, in the order it was resolved (≈ source order).
    pub list: Vec<Use>,
    /// Index from an occurrence's `(span.start, span.end)` to its slot in
    /// `list`. `start`/`end` alone identify an occurrence; `line`/`col` are
    /// derived and need not participate in the key.
    pub by_span: HashMap<(u32, u32), usize>,
}

impl Uses {
    /// Records a resolved occurrence and returns nothing; the use is appended to
    /// `list` and indexed by span. If two occurrences ever shared a span (they
    /// cannot, for distinct identifiers) the later one would win the index.
    pub fn record(&mut self, name: impl Into<String>, span: Span, res: Resolution) {
        let idx = self.list.len();
        self.list.push(Use {
            name: name.into(),
            span,
            res,
        });
        self.by_span.insert((span.start, span.end), idx);
    }

    /// Looks up the resolution recorded for the occurrence at `span`, if any.
    pub fn at(&self, span: Span) -> Option<&Use> {
        self.by_span
            .get(&(span.start, span.end))
            .map(|&i| &self.list[i])
    }
}
