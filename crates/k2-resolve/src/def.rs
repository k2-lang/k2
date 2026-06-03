//! Definitions: the binding sites that name resolution discovers.
//!
//! A [`Def`] records a single *declaration* — the place a name is introduced —
//! together with enough classification ([`DefKind`]) to drive lookup and
//! visibility. Uses (identifier *occurrences* that refer back to a `Def`) live
//! in [`crate::uses`]; the two are linked by [`DefId`].

use crate::ids::{DefId, ModuleId, ScopeId};
use k2_syntax::Span;

/// What a definition *is*. The kind determines lookup ordering, whether forward
/// references are allowed, and how an occurrence that resolves to it is
/// classified in the [`Uses`](crate::uses::Uses) table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefKind {
    /// A file-level or container-level item: a `const`, `var`, `fn`, or a
    /// nested type declaration (`const T = struct {...}`). Items are
    /// **order-independent** within their container — a `fn` may call a `fn`
    /// declared later, and a method may name a sibling member.
    Item,
    /// A function (or fn-type) parameter binding. Visible in the function body.
    Param,
    /// A block-local `const`/`var` binding. Locals are **order-dependent**: a
    /// use textually before the declaration does not see it.
    Local,
    /// A container *field* (struct/union field, or enum tag). Fields occupy the
    /// container member scope as declarations; their *uses* (member access) are
    /// deferred to type-checking, but a field still reserves its name.
    Field,
    /// A payload/loop/`errdefer`/`catch`/switch-arm capture binding (`|x|`).
    Capture,
    /// A module namespace bound by `@import` (a well-known name OR a path
    /// import). Member access on it (`std.heap.X`) resolves only the base.
    Module,
    /// A language-predeclared name: a primitive type, a capability type, or a
    /// C-interop alias. Lives in the root scope and is visible everywhere.
    Predeclared,
}

/// One definition (binding site).
#[derive(Clone, Debug)]
pub struct Def {
    /// This definition's own stable id (equal to its index in the table).
    pub id: DefId,
    /// What kind of binding this is.
    pub kind: DefKind,
    /// The bound name. For a discard (`_`) no `Def` is ever created, so this is
    /// always a real, non-empty identifier.
    pub name: String,
    /// The defining span. For [`DefKind::Predeclared`] this is a synthetic
    /// zero-width span at offset 0.
    pub span: Span,
    /// The scope this definition is declared *into* (its home namespace).
    pub scope: ScopeId,
    /// For [`DefKind::Module`]: which module-graph node this name denotes.
    /// `None` for every other kind.
    pub module: Option<ModuleId>,
    /// For [`DefKind::Item`]: whether the item is `pub`. Recorded for v0.5
    /// visibility checking; unused by v0.4 resolution itself.
    pub is_pub: bool,
}

/// The flat table of every definition, indexed by [`DefId`].
pub type DefTable = Vec<Def>;
