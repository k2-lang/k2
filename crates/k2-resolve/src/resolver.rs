//! The resolver walk: a two-pass-per-scope traversal of the AST that fills the
//! definition table, the scope tree, and the uses side-table, emitting precise
//! diagnostics along the way.
//!
//! ## The two-pass discipline
//!
//! Order-independent scopes (the file and every container body) are collected in
//! two passes: pass A inserts every member *name* so forward references work
//! (`fn a()` may call `fn b()` declared later; a method may name a sibling), and
//! pass B resolves each member's types/values/bodies against the now-complete
//! scope. Order-dependent scopes (blocks) are a single pass: a local is inserted
//! only *after* its own initializer is resolved, so it cannot see itself and a
//! use textually before the declaration resolves outward (or errors). Parameter
//! and capture scopes are collected eagerly — all params before the body, all
//! captures before the captured branch.
//!
//! ## The shadowing rule (R1) and the one place the spec is silent
//!
//! Spec §01 5.3 makes shadowing a *predeclared* name explicitly legal. The spec
//! is silent on local-vs-local / local-vs-outer shadowing, and the task directs:
//! "if the spec is silent, implement no-shadowing with a clear message and note
//! it." We therefore reject a declaration whose name is already bound in an
//! *enclosing user scope* (file/container/params/block/capture) as an illegal
//! shadow, while permitting shadowing of the predeclared root. Sibling
//! (non-nested) scopes reusing a name are not shadowing and are allowed — which
//! is exactly what the corpus relies on (`i`/`slot` across sibling `for`s, `err`
//! across sibling `catch`es). This is the single place the spec is silent; it is
//! documented here and on the crate root.

use crate::def::{Def, DefKind, DefTable};
use crate::diag::Diagnostic;
use crate::ids::{DefId, ModuleId, ScopeId};
use crate::module::{classify_import, ImportSpec, ModuleNode, ModuleRef};
use crate::predeclared::PREDECLARED;
use crate::scope::{Scope, ScopeKind, ScopeTree};
use crate::uses::{Resolution, Uses};
use k2_syntax::{
    Capture, CaptureName, Container, ContainerKind, Expr, ForOperand, InitBody, Item, Member,
    Param, SourceFile, Span, Stmt, SwitchArm, SwitchItem, SwitchPattern, UnionTag,
};

/// The fully resolved side-table for one source file.
pub struct Resolved {
    /// Every definition (binding site), indexed by [`DefId`].
    pub defs: DefTable,
    /// Every scope, indexed by [`ScopeId`].
    pub scopes: ScopeTree,
    /// Every identifier occurrence and what it resolved to.
    pub uses: Uses,
    /// The module-graph nodes introduced by this file's `@import`s.
    pub modules: Vec<ModuleNode>,
    /// The file (top-level) scope; its parent is the predeclared root.
    pub file_scope: ScopeId,
    /// Every diagnostic produced, in roughly source order.
    pub diagnostics: Vec<Diagnostic>,
}

impl Resolved {
    /// `true` if resolution produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.diagnostics.iter().all(|d| !d.is_error())
    }

    /// An iterator over just the error-severity diagnostics.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }
}

/// The mutable state of one in-progress resolution.
pub struct Resolver {
    defs: DefTable,
    scopes: ScopeTree,
    uses: Uses,
    modules: Vec<ModuleNode>,
    diags: Vec<Diagnostic>,
    /// The active scope stack; the innermost scope is last.
    stack: Vec<ScopeId>,
    /// The active label stack (block/loop labels). Labels are a *separate*
    /// namespace from value bindings and never clash with names.
    labels: Vec<(String, Span)>,
    /// Recursion-depth guard so adversarial deeply-nested input cannot blow the
    /// native stack; once exceeded, resolution stops descending.
    depth: u32,
    depth_exceeded: bool,
}

/// A generous cap on AST nesting depth. Real k2 never approaches this; it exists
/// purely so a pathological input cannot overflow the Rust call stack.
const MAX_DEPTH: u32 = 256;

impl Resolver {
    /// Builds a resolver with the predeclared root scope (id 0) and an empty
    /// file scope (id 1) already in place.
    pub fn new() -> Resolver {
        let mut r = Resolver {
            defs: Vec::new(),
            scopes: Vec::new(),
            uses: Uses::default(),
            modules: Vec::new(),
            diags: Vec::new(),
            stack: Vec::new(),
            labels: Vec::new(),
            depth: 0,
            depth_exceeded: false,
        };
        // Scope 0: the predeclared root.
        let root = r.new_scope(ScopeKind::Predeclared, None, Span::default());
        for &name in PREDECLARED {
            let id = r.alloc_def(
                DefKind::Predeclared,
                name,
                Span::default(),
                root,
                None,
                false,
            );
            r.scopes[root.index()].push(name, id);
        }
        // Scope 1: the file scope, a child of the predeclared root.
        let _file = r.new_scope(ScopeKind::File, Some(root), Span::default());
        r
    }

    /// Resolves a whole source file and consumes the resolver into a [`Resolved`].
    pub fn resolve(mut self, file: &SourceFile) -> Resolved {
        let file_scope = ScopeId(1);
        self.stack.push(file_scope);

        // Pass A: collect every top-level item name (order-independent).
        for item in &file.items {
            self.collect_item(file_scope, item);
        }
        // Pass B: resolve every item's types, values, and bodies.
        for item in &file.items {
            self.resolve_item(item);
        }

        self.stack.pop();
        Resolved {
            defs: self.defs,
            scopes: self.scopes,
            uses: self.uses,
            modules: self.modules,
            file_scope,
            diagnostics: self.diags,
        }
    }

    // =====================================================================
    //  Scope / definition primitives
    // =====================================================================

    /// Allocates a fresh scope and returns its id.
    fn new_scope(&mut self, kind: ScopeKind, parent: Option<ScopeId>, span: Span) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(Scope::new(id, kind, parent, span));
        id
    }

    /// Allocates a fresh definition and returns its id (does *not* file it into a
    /// scope's name list — callers that want a named binding use [`Self::declare`]).
    fn alloc_def(
        &mut self,
        kind: DefKind,
        name: &str,
        span: Span,
        scope: ScopeId,
        module: Option<ModuleId>,
        is_pub: bool,
    ) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(Def {
            id,
            kind,
            name: name.to_string(),
            span,
            scope,
            module,
            is_pub,
        });
        id
    }

    /// The innermost active scope.
    fn current_scope(&self) -> ScopeId {
        *self
            .stack
            .last()
            .expect("the scope stack is never empty during a walk")
    }

    /// Declares `name` into `scope`, emitting the duplicate / illegal-shadow
    /// diagnostics as appropriate. A discard (`_`) never becomes a binding and
    /// returns `None`. On a duplicate, the original definition is kept and the
    /// new one is dropped (still returning `None`), so later uses resolve to the
    /// first declaration.
    fn declare(
        &mut self,
        scope: ScopeId,
        name: &str,
        kind: DefKind,
        span: Span,
        is_pub: bool,
        module: Option<ModuleId>,
    ) -> Option<DefId> {
        // Discards never occupy a namespace; multiple `_` are always fine.
        if name == "_" || name.is_empty() {
            return None;
        }

        // R-dup: a duplicate in the *same* scope.
        if let Some(existing) = self.scopes[scope.index()].lookup_local(name) {
            // The spec-sanctioned exception: an `@import` const may share its
            // name with another top-level declaration (see `build.k2`, where
            // `const build = @import("build")` coexists with `fn build`). If
            // exactly one side is a module import, keep both silently and let
            // the non-module binding win for references.
            let existing_is_module = self.defs[existing.index()].kind == DefKind::Module;
            let new_is_module = kind == DefKind::Module;
            if existing_is_module ^ new_is_module {
                let new_id = self.alloc_def(kind, name, span, scope, module, is_pub);
                if existing_is_module {
                    // Replace the module binding so the real declaration wins.
                    self.scopes[scope.index()].repoint(name, new_id);
                }
                return Some(new_id);
            }

            let orig_span = self.defs[existing.index()].span;
            self.diags.push(
                Diagnostic::error(span, format!("redeclaration of `{name}` in this scope"))
                    .with_primary_label("redeclared here")
                    .with_secondary(orig_span, "first declared here"),
            );
            return None;
        }

        // R1: an illegal shadow of an enclosing *user* scope. We do not walk
        // into the current scope itself (that is the duplicate case above).
        if let Some(orig) = self.lookup_enclosing_user(scope, name) {
            let orig_span = self.defs[orig.index()].span;
            self.diags.push(
                Diagnostic::error(
                    span,
                    format!("declaration of `{name}` shadows an existing binding"),
                )
                .with_primary_label("shadows an outer binding")
                .with_secondary(orig_span, "the outer binding is declared here"),
            );
            return None;
        }

        let id = self.alloc_def(kind, name, span, scope, module, is_pub);
        self.scopes[scope.index()].push(name, id);
        Some(id)
    }

    /// Looks for `name` in every *enclosing* user scope of `scope` (its proper
    /// ancestors, excluding the predeclared root and `scope` itself). Returns the
    /// first match — the binding the new declaration would shadow.
    ///
    /// Two classes of *container-member* declaration are skipped, because both
    /// live in the member namespace and are only ever reached qualified
    /// (`self.field`, `Type.method`), never as a bare identifier — so an inner
    /// method's parameter or local may legitimately reuse such a name (matching
    /// the language k2 mirrors):
    ///
    /// * a container **field** (`generic_list.k2`: field `alloc` + param
    ///   `alloc`), and
    /// * a sibling container **item** — a `fn`/`const`/`var` declared directly in
    ///   the enclosing `struct`/`enum`/`union` body (`fn at(self, len: i32)`
    ///   alongside `fn len(self)`).
    ///
    /// **Crossing a `Container` boundary** is the third skip, and it is what makes
    /// the std-injection / multi-file merge sound. A `.k2` file IS a struct (spec
    /// §08.1), so a **file-level** item/module is *also* a member of an outer
    /// struct namespace, reached qualified (`std.eql`, `mod.NAME`), never as a
    /// bare identifier from inside a *sibling* nested namespace. The driver injects
    /// `std` (and `build` / `build_options` / every imported file) as nested
    /// `const __k2_..._root = struct { ... }`; their interior parameters and locals
    /// must NOT be flagged as "shadowing" a user's top-level `const eql = 5;` /
    /// `const a = 7;` / `const a = @import("./a.k2")`. We therefore stop treating a
    /// file-level `Item`/`Module` as shadowable once the outward walk has crossed
    /// into the enclosing struct's namespace from a nested container.
    ///
    /// What stays an error: a genuine local-vs-local, local-vs-param, or
    /// local-vs-outer-local shadow in `Params`/`Block`/`Capture` scopes
    /// (`illegal_shadow_param_by_local`), and a **direct** file-member function's
    /// local shadowing a file item with no intervening container in between
    /// (`illegal_shadow_of_file_item_by_local`).
    fn lookup_enclosing_user(&self, scope: ScopeId, name: &str) -> Option<DefId> {
        // Whether the outward walk has crossed a `Container` scope boundary on its
        // way to `s`. Once crossed, a file-level item/module is a member of the
        // enclosing struct, not a bare binding the new declaration can shadow. A
        // declaration *directly* in a `Container` (a sibling member of a nested
        // struct) is likewise in the member namespace, so it too may reuse a
        // file-level item/module name — seed `crossed_container` from the
        // declaring scope's own kind so a root re-export wrapper
        // (`const __k2_mod_<root> = struct { pub const V = V; }`) and a `@import`
        // back to the root resolve instead of spuriously "shadowing" it.
        let mut crossed_container = self.scopes[scope.index()].kind == ScopeKind::Container;
        let mut cur = self.scopes[scope.index()].parent;
        while let Some(s) = cur {
            let sc = &self.scopes[s.index()];
            if sc.kind.participates_in_shadow_check() {
                if let Some(id) = self.lookup_local_nonmember(s, sc.kind, name, crossed_container) {
                    return Some(id);
                }
            }
            if sc.kind == ScopeKind::Container {
                crossed_container = true;
            }
            cur = sc.parent;
        }
        None
    }

    /// Looks up `name` declared directly in `scope`, skipping declarations that
    /// occupy a *member* namespace rather than the bare-binding namespace (see
    /// [`Self::lookup_enclosing_user`]): container fields are always skipped;
    /// container *items* are skipped when `scope` is a `Container` body; and a
    /// **file**-level item/module is skipped once the outward walk has crossed a
    /// container boundary (`crossed_container`), because it then lives in an outer
    /// struct's member namespace rather than the bare-binding namespace.
    fn lookup_local_nonmember(
        &self,
        scope: ScopeId,
        kind: ScopeKind,
        name: &str,
        crossed_container: bool,
    ) -> Option<DefId> {
        let in_container = kind == ScopeKind::Container;
        let in_file = kind == ScopeKind::File;
        self.scopes[scope.index()]
            .names
            .iter()
            .find(|(n, id)| {
                if n != name {
                    return false;
                }
                match self.defs[id.index()].kind {
                    // Fields live in the member namespace everywhere.
                    DefKind::Field => false,
                    // Sibling container items (`fn`/`const`/`var`) are member-
                    // namespace too. A *file*-level item is normally a shadowable
                    // bare binding, but once the walk has crossed into the file
                    // struct from a nested container it is a member of that struct.
                    DefKind::Item => !(in_container || (in_file && crossed_container)),
                    // A file-level `@import` const (`std`, a path module) is a
                    // member of the file struct once reached across a container
                    // boundary — its name does not block a nested namespace's
                    // parameter/local of the same name.
                    DefKind::Module => !(in_file && crossed_container),
                    _ => true,
                }
            })
            .map(|(_, id)| *id)
    }

    /// Looks up `name` declared directly in `scope` for a *reference* (not a
    /// shadow check), skipping only container fields. Unlike
    /// [`Self::lookup_local_nonmember`], sibling container **items** are visible
    /// here — that is exactly what lets a method body name a sibling
    /// `fn`/`const` (`container_member_and_self`).
    fn lookup_local_nonfield(&self, scope: ScopeId, name: &str) -> Option<DefId> {
        self.scopes[scope.index()]
            .names
            .iter()
            .find(|(n, id)| n == name && self.defs[id.index()].kind != DefKind::Field)
            .map(|(_, id)| *id)
    }

    // =====================================================================
    //  Item collection (pass A) and resolution (pass B)
    // =====================================================================

    /// Pass A for one item: declare its name (if it has one) into `scope`.
    /// `const`/`var`/`fn` bind a name; `test`/`comptime` declare none. A
    /// `const X = @import(...)` binds a *module* and registers a graph node.
    fn collect_item(&mut self, scope: ScopeId, item: &Item) {
        match item {
            Item::Const {
                name,
                value,
                is_pub,
                span,
                ..
            } => {
                if let Some(module) = self.import_of(value) {
                    let mid = self.intern_module(module, *span);
                    self.declare(scope, name, DefKind::Module, *span, *is_pub, Some(mid));
                } else {
                    self.declare(scope, name, DefKind::Item, *span, *is_pub, None);
                }
            }
            Item::Var {
                name, is_pub, span, ..
            } => {
                self.declare(scope, name, DefKind::Item, *span, *is_pub, None);
            }
            Item::Fn {
                name, is_pub, span, ..
            } => {
                self.declare(scope, name, DefKind::Item, *span, *is_pub, None);
            }
            Item::Test { .. } | Item::Comptime { .. } => {
                // No name is bound by a `test` or a top-level `comptime` block.
            }
        }
    }

    /// Pass B for one item: resolve its types, values, and bodies.
    fn resolve_item(&mut self, item: &Item) {
        match item {
            Item::Const { ty, value, .. } => {
                if let Some(ty) = ty {
                    self.resolve_expr(ty);
                }
                // An `@import(...)` value carries only a string literal; do not
                // descend (the string is not an identifier use).
                if self.import_of(value).is_none() {
                    self.resolve_expr(value);
                }
            }
            Item::Var { ty, value, .. } => {
                if let Some(ty) = ty {
                    self.resolve_expr(ty);
                }
                if let Some(value) = value {
                    self.resolve_expr(value);
                }
            }
            Item::Fn {
                params, ret, body, ..
            } => {
                self.resolve_fn(params, ret, body.as_deref());
            }
            Item::Test { body, .. } => {
                let block = self.open_block(Span::default());
                self.resolve_block_stmts(block, body);
                self.close_scope();
            }
            Item::Comptime { body, .. } => {
                let block = self.open_block(Span::default());
                self.resolve_block_stmts(block, body);
                self.close_scope();
            }
        }
    }

    /// Resolves a function: a parameter scope holding the params, with the body
    /// block nested inside it.
    ///
    /// Parameter types are resolved left-to-right *inside* the params scope, each
    /// param being bound before the next param's type is resolved. This is what
    /// lets a later parameter's type name an earlier `comptime` parameter — e.g.
    /// `fn printFields(comptime T: type, out: anytype, value: T)`, where the type
    /// of `value` is the earlier param `T`. The return type is resolved last,
    /// with every param in scope (`fn f(comptime T: type) T`), and the body
    /// nests inside the params scope.
    fn resolve_fn(&mut self, params: &[Param], ret: &Expr, body: Option<&[Stmt]>) {
        let params_scope = self.new_scope(
            ScopeKind::Params,
            Some(self.current_scope()),
            Span::default(),
        );
        self.stack.push(params_scope);
        for p in params {
            // Resolve this param's type against the params seen so far, then
            // bind the param so a later param's type can reference it.
            self.resolve_expr(&p.ty);
            self.declare(params_scope, &p.name, DefKind::Param, p.span, false, None);
        }
        self.resolve_expr(ret);
        if let Some(body) = body {
            let block = self.open_block(Span::default());
            self.resolve_block_stmts(block, body);
            self.close_scope();
        }
        self.stack.pop(); // params_scope
    }

    // =====================================================================
    //  Blocks and statements
    // =====================================================================

    /// Opens a fresh block scope as a child of the current scope and pushes it.
    fn open_block(&mut self, span: Span) -> ScopeId {
        let s = self.new_scope(ScopeKind::Block, Some(self.current_scope()), span);
        self.stack.push(s);
        s
    }

    /// Pops the innermost scope.
    fn close_scope(&mut self) {
        self.stack.pop();
    }

    /// Resolves a sequence of statements into the already-open block `scope`.
    fn resolve_block_stmts(&mut self, scope: ScopeId, stmts: &[Stmt]) {
        for s in stmts {
            self.resolve_stmt(scope, s);
        }
    }

    /// Resolves one statement, declaring any locals it introduces into `scope`.
    fn resolve_stmt(&mut self, scope: ScopeId, stmt: &Stmt) {
        if !self.enter() {
            return;
        }
        match stmt {
            Stmt::Const {
                name,
                ty,
                value,
                span,
            } => {
                if let Some(ty) = ty {
                    self.resolve_expr(ty);
                }
                self.resolve_expr(value);
                // Bind *after* resolving the initializer (order-dependent).
                self.declare(scope, name, DefKind::Local, *span, false, None);
            }
            Stmt::Var {
                name,
                ty,
                value,
                span,
            } => {
                if let Some(ty) = ty {
                    self.resolve_expr(ty);
                }
                if let Some(value) = value {
                    self.resolve_expr(value);
                }
                self.declare(scope, name, DefKind::Local, *span, false, None);
            }
            Stmt::Defer { body, .. } => {
                // A `defer` body is its own nested block.
                let block = self.open_block(body.span());
                self.resolve_stmt(block, body);
                self.close_scope();
            }
            Stmt::Errdefer { capture, body, .. } => {
                // `errdefer |e| <stmt>` binds the capture for the body only.
                let cap =
                    self.new_scope(ScopeKind::Capture, Some(self.current_scope()), body.span());
                self.stack.push(cap);
                if let Some(name) = capture {
                    self.declare(cap, name, DefKind::Capture, body.span(), false, None);
                }
                let block = self.open_block(body.span());
                self.resolve_stmt(block, body);
                self.close_scope(); // block
                self.stack.pop(); // capture
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.resolve_expr(value);
                }
            }
            Stmt::Expr { expr, .. } => self.resolve_expr(expr),
            Stmt::Assign { target, value, .. } => {
                self.resolve_expr(value);
                self.resolve_assign_target(target);
            }
            Stmt::Comptime { body, span } => {
                let block = self.open_block(*span);
                self.resolve_block_stmts(block, body);
                self.close_scope();
            }
            Stmt::Block { body, span } => {
                let block = self.open_block(*span);
                self.resolve_block_stmts(block, body);
                self.close_scope();
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => self.resolve_expr(expr),
            Stmt::Break { label, value, span } => {
                self.check_label(label, *span);
                if let Some(value) = value {
                    self.resolve_expr(value);
                }
            }
            Stmt::Continue { label, span } => {
                self.check_label(label, *span);
            }
        }
        self.leave();
    }

    /// Resolves an assignment target. The discard sink `_` is valid and records
    /// nothing; any other target is an ordinary lvalue expression.
    fn resolve_assign_target(&mut self, target: &Expr) {
        if let Expr::Ident { name, .. } = target {
            if name == "_" {
                return; // the discard sink: no binding, no use, no error.
            }
        }
        self.resolve_expr(target);
    }

    // =====================================================================
    //  Expressions
    // =====================================================================

    /// Resolves an expression, recording uses and opening any scopes it needs.
    fn resolve_expr(&mut self, expr: &Expr) {
        if !self.enter() {
            return;
        }
        match expr {
            // ---- Leaves with no uses ------------------------------------
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Char { .. }
            | Expr::Bool { .. }
            | Expr::Null { .. }
            | Expr::Undefined { .. }
            | Expr::Unreachable { .. }
            | Expr::AnyType { .. } => {}

            // The core use: an identifier reference.
            Expr::Ident { name, span } => self.resolve_ident(name, *span),

            // ---- Deferred member positions ------------------------------
            Expr::Field { base, span, .. } => {
                // Resolve only the base; the field name is deferred to v0.5.
                self.resolve_expr(base);
                self.uses.record("", *span, Resolution::DeferredMember);
            }
            Expr::EnumLiteral { span, .. } | Expr::ErrorLiteral { span, .. } => {
                // `.Name` / `error.Name`: the member name is deferred.
                self.uses.record("", *span, Resolution::DeferredMember);
            }

            // ---- Builtins -----------------------------------------------
            Expr::Builtin { args, .. } => {
                // The builtin *name* (`@import`, `@as`, …) is known to the lexer
                // and is never an identifier use. The arguments are ordinary
                // expressions (`@as(u32, x)` resolves `u32` and `x`); a string
                // argument (as in `@import("std")`) simply records nothing.
                for a in args {
                    self.resolve_expr(a);
                }
            }

            // ---- Calls / postfix / operators ----------------------------
            Expr::Call { callee, args, .. } => {
                self.resolve_expr(callee);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            Expr::Index { base, index, .. } => {
                self.resolve_expr(base);
                self.resolve_expr(index);
            }
            Expr::SliceExpr { base, lo, hi, .. } => {
                self.resolve_expr(base);
                self.resolve_expr(lo);
                if let Some(hi) = hi {
                    self.resolve_expr(hi);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs);
                self.resolve_expr(rhs);
            }
            Expr::Unary { operand, .. } => self.resolve_expr(operand),
            Expr::Optional { inner, .. } => self.resolve_expr(inner),
            Expr::Pointer { align, inner, .. } | Expr::Slice { align, inner, .. } => {
                if let Some(a) = align {
                    self.resolve_expr(a);
                }
                self.resolve_expr(inner);
            }
            Expr::ManyPtr {
                sentinel, inner, ..
            } => {
                if let Some(s) = sentinel {
                    self.resolve_expr(s);
                }
                self.resolve_expr(inner);
            }
            Expr::ArrayType { len, inner, .. } => {
                self.resolve_expr(len);
                self.resolve_expr(inner);
            }
            Expr::ErrorUnion { err, ok, .. } => {
                if let Some(err) = err {
                    self.resolve_expr(err);
                }
                self.resolve_expr(ok);
            }
            Expr::FnType { params, ret, .. } => {
                // A function *type* introduces a transient param scope. Its
                // params are usually unnamed (`""`), so few bindings result.
                // Resolve each param type then bind it, so a later param's type
                // may reference an earlier (`comptime`) one, matching `fn` decls.
                let ps = self.new_scope(
                    ScopeKind::Params,
                    Some(self.current_scope()),
                    Span::default(),
                );
                self.stack.push(ps);
                for p in params {
                    self.resolve_expr(&p.ty);
                    self.declare(ps, &p.name, DefKind::Param, p.span, false, None);
                }
                self.resolve_expr(ret);
                self.stack.pop();
            }
            Expr::ErrorSet { .. } => {
                // The member names declare error *values*, not uses; deferred to
                // the type layer. Nothing to resolve here.
            }
            Expr::Deref { base, .. } | Expr::Unwrap { base, .. } => self.resolve_expr(base),
            Expr::Comptime { inner, .. } => self.resolve_expr(inner),
            Expr::Catch {
                lhs, capture, rhs, ..
            } => {
                self.resolve_expr(lhs);
                let cap =
                    self.new_scope(ScopeKind::Capture, Some(self.current_scope()), rhs.span());
                self.stack.push(cap);
                if let Some(name) = capture {
                    self.declare(cap, name, DefKind::Capture, rhs.span(), false, None);
                }
                self.resolve_expr(rhs);
                self.stack.pop();
            }

            // ---- Containers ---------------------------------------------
            Expr::Container(c) => self.resolve_container(c),

            // ---- Initializers -------------------------------------------
            Expr::Init { ty, body, .. } => {
                if let Some(ty) = ty {
                    self.resolve_expr(ty);
                }
                match body {
                    InitBody::Fields(fields) => {
                        for f in fields {
                            // The field *name* is deferred; the value resolves.
                            self.uses.record("", f.span, Resolution::DeferredMember);
                            self.resolve_expr(&f.value);
                        }
                    }
                    InitBody::Tuple(elems) => {
                        for e in elems {
                            self.resolve_expr(e);
                        }
                    }
                }
            }

            // ---- Control flow in expression position --------------------
            Expr::Block { label, body, span } => {
                self.push_label(label, *span);
                let block = self.open_block(*span);
                self.resolve_block_stmts(block, body);
                self.close_scope();
                self.pop_label(label);
            }
            Expr::If {
                cond,
                capture,
                then_branch,
                else_capture,
                else_branch,
                ..
            } => {
                self.resolve_expr(cond);
                self.resolve_branch(capture.as_ref(), then_branch);
                if let Some(else_branch) = else_branch {
                    self.resolve_branch(else_capture.as_ref(), else_branch);
                }
            }
            Expr::While {
                label,
                cond,
                capture,
                cont,
                body,
                else_capture,
                else_branch,
                span,
                ..
            } => {
                self.resolve_expr(cond);
                self.push_label(label, *span);
                // The capture is in scope for the continuation and the body.
                let cap = self.new_scope(ScopeKind::Capture, Some(self.current_scope()), *span);
                self.stack.push(cap);
                self.bind_capture(cap, capture.as_ref());
                if let Some(cont) = cont {
                    let block = self.open_block(cont.span());
                    self.resolve_stmt(block, cont);
                    self.close_scope();
                }
                self.resolve_expr(body);
                self.stack.pop(); // capture
                self.pop_label(label);
                if let Some(else_branch) = else_branch {
                    self.resolve_branch(else_capture.as_ref(), else_branch);
                }
            }
            Expr::For {
                label,
                operands,
                captures,
                body,
                else_branch,
                span,
                ..
            } => {
                // Operands are resolved in the enclosing scope.
                for op in operands {
                    match op {
                        ForOperand::Value(e) => self.resolve_expr(e),
                        ForOperand::Range { lo, hi, .. } => {
                            self.resolve_expr(lo);
                            if let Some(hi) = hi {
                                self.resolve_expr(hi);
                            }
                        }
                    }
                }
                self.push_label(label, *span);
                let cap = self.new_scope(ScopeKind::Capture, Some(self.current_scope()), *span);
                self.stack.push(cap);
                for c in captures {
                    self.bind_capture_name(cap, c);
                }
                self.resolve_expr(body);
                self.stack.pop();
                self.pop_label(label);
                // The `else` branch sees no captures.
                if let Some(else_branch) = else_branch {
                    self.resolve_expr(else_branch);
                }
            }
            Expr::Switch {
                scrutinee, arms, ..
            } => {
                self.resolve_expr(scrutinee);
                for arm in arms {
                    self.resolve_switch_arm(arm);
                }
            }
        }
        self.leave();
    }

    /// Resolves an `if`/`else` branch (its optional capture binds for the branch
    /// body only).
    fn resolve_branch(&mut self, capture: Option<&Capture>, body: &Expr) {
        let cap = self.new_scope(ScopeKind::Capture, Some(self.current_scope()), body.span());
        self.stack.push(cap);
        self.bind_capture(cap, capture);
        self.resolve_expr(body);
        self.stack.pop();
    }

    /// Resolves one `switch` arm: its pattern items (ordinary expressions) in the
    /// enclosing scope, then its body with the arm capture in scope.
    fn resolve_switch_arm(&mut self, arm: &SwitchArm) {
        if let SwitchPattern::Items(items) = &arm.pattern {
            for SwitchItem { lo, hi, .. } in items {
                self.resolve_expr(lo);
                if let Some(hi) = hi {
                    self.resolve_expr(hi);
                }
            }
        }
        let cap = self.new_scope(ScopeKind::Capture, Some(self.current_scope()), arm.span);
        self.stack.push(cap);
        self.bind_capture(cap, arm.capture.as_ref());
        self.resolve_expr(&arm.body);
        self.stack.pop();
    }

    /// Binds every name of a `|a, b|` capture into `scope` (discards skipped).
    fn bind_capture(&mut self, scope: ScopeId, capture: Option<&Capture>) {
        if let Some(c) = capture {
            for name in &c.names {
                self.bind_capture_name(scope, name);
            }
        }
    }

    /// Binds a single capture name (`|x|` / `|*slot|`) into `scope`.
    fn bind_capture_name(&mut self, scope: ScopeId, name: &CaptureName) {
        self.declare(scope, &name.name, DefKind::Capture, name.span, false, None);
    }

    // =====================================================================
    //  Containers
    // =====================================================================

    /// Resolves a container type body (`struct`/`enum`/`union`). The tag
    /// expression resolves in the *enclosing* scope; the member scope holds the
    /// fields and nested declarations (order-independent, two passes) plus any
    /// user-declared `Self`.
    fn resolve_container(&mut self, c: &Container) {
        // The kind's tag/extern data is resolved in the enclosing scope.
        match &c.kind {
            ContainerKind::Struct { .. } => {}
            ContainerKind::Enum { tag } => {
                if let Some(tag) = tag {
                    self.resolve_expr(tag);
                }
            }
            ContainerKind::Union { tag } => {
                if let UnionTag::Typed(t) = tag {
                    self.resolve_expr(t);
                }
            }
        }

        let scope = self.new_scope(ScopeKind::Container, Some(self.current_scope()), c.span);
        self.stack.push(scope);

        // Pass A: collect every field name and every nested declaration name.
        for m in &c.members {
            match m {
                Member::Field(f) => {
                    self.declare(scope, &f.name, DefKind::Field, f.span, f.is_pub, None);
                }
                Member::Decl(item) => {
                    self.collect_item(scope, item);
                }
            }
        }
        // Pass B: resolve field types/defaults and nested declaration bodies.
        for m in &c.members {
            match m {
                Member::Field(f) => {
                    if let Some(ty) = &f.ty {
                        self.resolve_expr(ty);
                    }
                    if let Some(align) = &f.align {
                        self.resolve_expr(align);
                    }
                    if let Some(default) = &f.default {
                        self.resolve_expr(default);
                    }
                }
                Member::Decl(item) => {
                    self.resolve_item(item);
                }
            }
        }

        self.stack.pop();
    }

    // =====================================================================
    //  Identifier lookup
    // =====================================================================

    /// Resolves a bare identifier reference, walking the scope stack outward.
    /// Records the result as a [`Use`](crate::uses::Use); an unresolved name
    /// emits an "undeclared identifier" diagnostic.
    fn resolve_ident(&mut self, name: &str, span: Span) {
        // `_` is the discard; it never denotes a binding. It only appears in
        // binding/target position in real k2, but if one reaches here treat it
        // as a non-resolving placeholder rather than an error.
        if name == "_" {
            self.uses.record(name, span, Resolution::DeferredMember);
            return;
        }

        // Walk innermost -> outermost. Container fields are skipped: a bare
        // identifier never denotes a field (fields are reached as `self.field`),
        // so a method body's reference to a name that also happens to be a field
        // resolves past it to an enclosing binding (or errors).
        let mut cur = Some(self.current_scope());
        while let Some(s) = cur {
            if let Some(id) = self.lookup_local_nonfield(s, name) {
                let res = match self.defs[id.index()].kind {
                    DefKind::Predeclared => Resolution::Predeclared(id),
                    DefKind::Module => Resolution::Module(id),
                    _ => Resolution::Def(id),
                };
                self.uses.record(name, span, res);
                return;
            }
            cur = self.scopes[s.index()].parent;
        }

        // The arbitrary-width integer family `uN` / `iN` is an open *pattern*,
        // not an enumerated list (§07's parity table uses `[256]u1`, `u1`):
        // recognize it here, just before reporting an undeclared name, and
        // resolve it as a predeclared primitive. The first time a given width is
        // seen we lazily file a `Def` for it into the predeclared root scope, so
        // a later use of the same width is found by the ordinary outward walk
        // above and shares the same `DefId`.
        if primitive_int_width(name).is_some() {
            let id = self
                .scopes
                .first()
                .and_then(|root| root.lookup_local(name))
                .unwrap_or_else(|| {
                    let root = ScopeId(0);
                    let id = self.alloc_def(
                        DefKind::Predeclared,
                        name,
                        Span::default(),
                        root,
                        None,
                        false,
                    );
                    self.scopes[root.index()].push(name, id);
                    id
                });
            self.uses.record(name, span, Resolution::Predeclared(id));
            return;
        }

        // Build the rich diagnostic: a primary label under the ident, plus a
        // best-effort "did you mean" help drawn from the names actually visible
        // in scope (cheap edit-distance, pure std).
        let mut diag = Diagnostic::error(span, format!("use of undeclared identifier `{name}`"))
            .with_primary_label("not found in this scope");
        if let Some(near) = self.nearest_in_scope(name) {
            diag = diag.with_help(format!(
                "a binding named `{near}` exists — did you mean it?"
            ));
        }
        self.diags.push(diag);
        self.uses.record(name, span, Resolution::Error);
    }

    /// Finds the nearest in-scope binding name to `name` by Levenshtein edit
    /// distance, returning it only when the distance is small (`<= 2` and at most
    /// half the name's length). Used to power the "did you mean" suggestion on an
    /// undeclared identifier. Pure std, no external crate.
    fn nearest_in_scope(&self, name: &str) -> Option<String> {
        let max = (name.chars().count() / 2).clamp(1, 2);
        let mut best: Option<(usize, &str)> = None;
        let mut cur = Some(self.current_scope());
        while let Some(s) = cur {
            for (cand, _id) in &self.scopes[s.index()].names {
                if cand == name || cand == "_" {
                    continue;
                }
                let d = edit_distance(name, cand);
                if d <= max && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, cand.as_str()));
                }
            }
            cur = self.scopes[s.index()].parent;
        }
        best.map(|(_, n)| n.to_string())
    }

    // =====================================================================
    //  Labels (a separate namespace from value bindings)
    // =====================================================================

    /// Pushes a block/loop label if present, reporting a label that shadows an
    /// enclosing label of the same name.
    fn push_label(&mut self, label: &Option<String>, span: Span) {
        if let Some(l) = label {
            if self.labels.iter().any(|(n, _)| n == l) {
                self.diags.push(Diagnostic::error(
                    span,
                    format!("label `{l}` shadows an enclosing label"),
                ));
            }
            self.labels.push((l.clone(), span));
        }
    }

    /// Pops the most recently pushed label, if this construct pushed one.
    fn pop_label(&mut self, label: &Option<String>) {
        if label.is_some() {
            self.labels.pop();
        }
    }

    /// Checks that a `break :l` / `continue :l` names a label currently in
    /// scope.
    fn check_label(&mut self, label: &Option<String>, span: Span) {
        if let Some(l) = label {
            if !self.labels.iter().any(|(n, _)| n == l) {
                self.diags.push(Diagnostic::error(
                    span,
                    format!("use of undeclared label `{l}`"),
                ));
            }
        }
    }

    // =====================================================================
    //  Imports
    // =====================================================================

    /// If `value` is exactly `@import("...")`, returns the classified import.
    fn import_of(&self, value: &Expr) -> Option<ImportSpec> {
        if let Expr::Builtin { name, args, .. } = value {
            if name == "@import" {
                if let [Expr::Str { text, .. }] = args.as_slice() {
                    let raw = strip_quotes(text);
                    return Some(classify_import(&raw));
                }
            }
        }
        None
    }

    /// Interns a module (deduplicating by reference) and returns its id.
    fn intern_module(&mut self, spec: ImportSpec, origin: Span) -> ModuleId {
        let reference = match spec {
            ImportSpec::Named(n) => ModuleRef::WellKnown(n),
            // In single-file resolution path imports are recorded as nodes but
            // never followed; the multi-file driver replaces/augments these.
            ImportSpec::Path(p) => ModuleRef::Path(std::path::PathBuf::from(p)),
        };
        if let Some(existing) = self.modules.iter().find(|m| m.reference == reference) {
            return existing.id;
        }
        let id = ModuleId(self.modules.len() as u32);
        self.modules.push(ModuleNode {
            id,
            reference,
            origin,
        });
        id
    }

    // =====================================================================
    //  Depth guard
    // =====================================================================

    /// Enters one level of recursion; returns `false` (and records a one-shot
    /// diagnostic) once the depth cap is exceeded, telling the caller to stop
    /// descending. This protects the native stack from adversarial nesting.
    fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            if !self.depth_exceeded {
                self.depth_exceeded = true;
                self.diags.push(Diagnostic::error(
                    Span::default(),
                    "expression/statement nesting too deep to resolve",
                ));
            }
            self.depth -= 1;
            return false;
        }
        true
    }

    /// Leaves one level of recursion.
    fn leave(&mut self) {
        self.depth -= 1;
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Resolver::new()
    }
}

/// The maximum bit width of an arbitrary-width primitive integer. k2 mirrors
/// Zig here, where `u0`..=`u65535` and `i0`..=`i65535` are valid; a wider
/// request (`u70000`) is not a primitive and falls through to the ordinary
/// "undeclared identifier" path.
const MAX_INT_WIDTH: u32 = 65535;

/// If `name` is a primitive arbitrary-width integer type — a single leading
/// `u` or `i` followed by a canonical decimal width in `0..=MAX_INT_WIDTH` —
/// returns that width. Otherwise returns `None`.
///
/// The width must be *canonical*: digits only, with no leading zero unless the
/// width is the single digit `0` (so `u0` is accepted but `u01` is rejected),
/// matching how the language spells these names. The width is bounded by
/// [`MAX_INT_WIDTH`]; anything larger (or with an over-long digit run) is not a
/// primitive. Named widths like `usize`/`isize` are *not* matched here — they
/// stay enumerated predeclared entries and never reach this helper, because the
/// digit check fails on the `size` suffix.
fn primitive_int_width(name: &str) -> Option<u32> {
    let digits = match name.as_bytes().split_first() {
        Some((b'u' | b'i', rest)) if !rest.is_empty() => rest,
        _ => return None,
    };
    // All remaining bytes must be ASCII digits.
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Reject a non-canonical leading zero (`u01`), but accept the lone `0`.
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    // Parse with the bound applied. A digit run too long to be `<= 65535`
    // overflows the parse or exceeds the cap, and is rejected as not-a-primitive.
    let width: u32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (width <= MAX_INT_WIDTH).then_some(width)
}

/// The Levenshtein edit distance between two strings (insertions, deletions,
/// substitutions all cost 1), computed over Unicode scalars with the classic
/// two-row dynamic-programming table. Pure std; used to power the undeclared-
/// identifier "did you mean" suggestion. O(len(a) * len(b)) time, O(len(b))
/// space.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Strips one layer of surrounding double quotes from a string-literal lexeme,
/// returning the inner text. Leaves an already-unquoted string untouched.
fn strip_quotes(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}
