//! Stable index newtypes used throughout the resolved side-table.
//!
//! Every definition, scope, and module is addressed by a small `u32`-backed
//! index rather than a pointer or a borrow, so the four parallel tables that
//! make up a [`Resolved`](crate::Resolved) result can reference one another
//! freely without lifetime entanglement. The newtypes are `Copy` and totally
//! ordered, which keeps dumps deterministic and makes the indices cheap to pass
//! around and store in maps.

/// A stable index into the definition table (`DefTable`).
///
/// `DefId(0)` is not special; definitions are numbered in creation order, which
/// for the predeclared scope is fixed and for user code follows source order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DefId(pub u32);

/// A stable index into the scope tree (`ScopeTree`).
///
/// By construction `ScopeId(0)` is always the predeclared root scope and
/// `ScopeId(1)` is always the file scope (its child); every other scope is a
/// descendant of the file scope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ScopeId(pub u32);

/// A stable index into the module-graph node list.
///
/// Modules are interned: importing the same well-known name or the same
/// canonical path twice yields the same `ModuleId`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ModuleId(pub u32);

impl DefId {
    /// The underlying index, for table lookups (`&defs[id.index()]`).
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl ScopeId {
    /// The underlying index, for table lookups (`&scopes[id.index()]`).
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl ModuleId {
    /// The underlying index, for table lookups (`&modules[id.index()]`).
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
