//! Lexical scopes: the nested namespaces that bindings live in.
//!
//! A [`Scope`] is one node of the scope tree. Each scope owns an
//! insertion-ordered list of the names declared *directly* in it (mapping each
//! to its [`DefId`]) and points at its lexical parent. Name lookup walks from an
//! innermost scope outward through `parent` links until it reaches the
//! predeclared root.
//!
//! The insertion-ordered `names` `Vec<(String, DefId)>` is the source of truth
//! for *deterministic dumps*. Alongside it each scope keeps an `index`
//! (`HashMap<String, DefId>`) that mirrors `names` purely as a fast membership
//! probe: same-scope duplicate detection (which doubles as the direct-lookup
//! primitive) must be O(1) amortized, not a linear scan, so a very wide
//! file/container scope resolves in linear — not quadratic — time. The two are
//! always maintained together via [`Scope::push`].

use crate::ids::{DefId, ScopeId};
use k2_syntax::Span;
use std::collections::HashMap;

/// The kind of a scope. The kind drives two things: the lookup/visibility rules
/// (e.g. only non-predeclared scopes participate in shadow checks) and whether
/// the scope is collected in two passes (order-independent) or one
/// (order-dependent).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeKind {
    /// The synthetic root holding every predeclared name (primitives, the
    /// capability types, the C-interop aliases). Its names may be shadowed.
    Predeclared,
    /// The file top-level (the implicit file struct). Items are
    /// order-independent; forward references between them are allowed.
    File,
    /// A container body (`struct`/`enum`/`union`). Members (fields + nested
    /// decls + `Self`) are order-independent and methods see their siblings.
    Container,
    /// A function parameter scope, sitting between the enclosing
    /// file/container scope and the function body block.
    Params,
    /// A `{ ... }` block or function body. Locals are order-dependent.
    Block,
    /// A capture scope: the binders introduced by `if`/`while`/`for`/`catch`/
    /// `errdefer`/`switch`-arm payloads (`|x|`, `|*slot, i|`, `|err|`).
    Capture,
}

impl ScopeKind {
    /// `true` if a *new* declaration entering an enclosing scope of this kind
    /// should be rejected as an illegal shadow. Every user scope counts;
    /// shadowing a predeclared name is explicitly permitted by spec §01 5.3.
    pub fn participates_in_shadow_check(self) -> bool {
        !matches!(self, ScopeKind::Predeclared)
    }
}

/// One lexical scope.
#[derive(Clone, Debug)]
pub struct Scope {
    /// This scope's own stable id (equal to its index in the tree).
    pub id: ScopeId,
    /// What kind of scope this is.
    pub kind: ScopeKind,
    /// The lexically enclosing scope, or `None` for the predeclared root.
    pub parent: Option<ScopeId>,
    /// Names declared directly in this scope, in insertion order, each mapped
    /// to its definition. Insertion order keeps dumps deterministic.
    pub names: Vec<(String, DefId)>,
    /// A membership index over `names` for O(1) amortized direct lookup /
    /// duplicate detection. Maps each declared name to the [`DefId`] of its
    /// *first* declaration — the same `DefId` `names` records for it — so a
    /// duplicate is detected without scanning the `Vec`. Kept private and in
    /// lockstep with `names` via [`Scope::push`].
    index: HashMap<String, DefId>,
    /// The span of the syntactic construct that opened this scope (for dumps).
    pub span: Span,
}

impl Scope {
    /// Builds an empty scope with both `names` and its membership `index`
    /// initialized.
    pub fn new(id: ScopeId, kind: ScopeKind, parent: Option<ScopeId>, span: Span) -> Scope {
        Scope {
            id,
            kind,
            parent,
            names: Vec::new(),
            index: HashMap::new(),
            span,
        }
    }

    /// Looks up `name` declared *directly* in this scope (no parent walk). O(1)
    /// amortized via the membership `index`.
    pub fn lookup_local(&self, name: &str) -> Option<DefId> {
        self.index.get(name).copied()
    }

    /// Files a fresh `(name, id)` binding into this scope, updating both the
    /// ordered `names` list and the membership `index`. The caller must have
    /// already established that `name` is not a same-scope duplicate (e.g. via
    /// [`Self::lookup_local`]); this is the single place the two structures are
    /// grown in lockstep.
    pub fn push(&mut self, name: &str, id: DefId) {
        self.names.push((name.to_string(), id));
        self.index.insert(name.to_string(), id);
    }

    /// Repoints the binding for `name` (already present) at a new [`DefId`] in
    /// both the ordered list and the membership index. Used for the `@import`
    /// const / real-declaration name-sharing carve-out, where the real
    /// declaration must win over a previously filed module binding.
    pub fn repoint(&mut self, name: &str, id: DefId) {
        if let Some(slot) = self.names.iter_mut().find(|(n, _)| n == name) {
            slot.1 = id;
        }
        self.index.insert(name.to_string(), id);
    }
}

/// The flat tree of every scope, indexed by [`ScopeId`]. Parent/child structure
/// is encoded by the `parent` field on each [`Scope`].
pub type ScopeTree = Vec<Scope>;
