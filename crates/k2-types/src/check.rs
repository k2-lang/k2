//! The bidirectional checker: the [`Checker`] state plus `synth`/`check` over
//! expressions, and the item/statement walkers.
//!
//! Checking has two mutually-recursive primitives. [`Checker::synth`] infers an
//! expression's type bottom-up; [`Checker::check`] checks an expression against
//! an *expected* type, special-casing the literals and initializers that need
//! the expectation (integer literals, `null`, enum/error literals, anonymous
//! initializers, and the branch-joining control-flow forms). Every typed node's
//! type is recorded by span into [`Typed::types`](crate::Typed::types) for the
//! per-occurrence dump and tooling.
//!
//! The checker walks the [`Resolved`] side-table and never re-resolves names:
//! an `Expr::Ident` is looked up via `Resolved::uses.at(span)`; a member/field/
//! enum/error position the resolver left as `DeferredMember` is resolved here
//! against the now-known base type (see [`crate::member`]).

use std::collections::{HashMap, HashSet};

use k2_resolve::{DefId, DefKind, Resolution, Resolved};
use k2_syntax::{
    AssignOp, Capture, Container, ContainerKind, Expr, ForOperand, Item, Member, Param, SourceFile,
    Span, Stmt, UnionTag,
};

use crate::arena::TypeArena;
use crate::diag::Diagnostic;
use crate::ty::{
    EnumInfo, EnumVariant, ErrSetRef, FieldInfo, FnSig, MemberDecl, MemberRes, ParamInfo,
    StructInfo, Type, TypeId, UnionInfo, UnionTagKind, UnionVariant,
};
use crate::Typed;

/// A generous cap on AST nesting depth, mirroring the resolver's guard, so a
/// pathological input cannot overflow the native stack.
const MAX_DEPTH: u32 = 256;

/// One enclosing-function frame: its declared return type and the facts the
/// `return`/`try` rules need.
pub(crate) struct FnFrame {
    /// The declared return type.
    pub ret: TypeId,
    /// `true` if `ret` is an error union (or an inferred `!T`), so `try` is
    /// allowed and a bare `return;` may carry no value when the ok type is void.
    pub ret_is_error_union: bool,
    /// The ok payload of an error-union return (or `ret` itself otherwise).
    pub ret_ok: TypeId,
    /// The explicit error set of an error-union return, for `try` set-superset
    /// checks. `None` for an inferred/any/non-union return.
    pub ret_err: ErrSetRef,
    /// `true` if the function body contained at least one value `return`.
    pub saw_value_return: bool,
}

/// The mutable state of one in-progress type-check.
pub struct Checker<'a> {
    /// The resolved side-table this check is keyed against.
    pub(crate) resolved: &'a Resolved,
    /// The interning type arena.
    pub(crate) arena: TypeArena,
    /// Inferred type per occurrence span.
    pub(crate) types: HashMap<(u32, u32), TypeId>,
    /// Resolved member per previously-deferred occurrence span.
    pub(crate) members: HashMap<(u32, u32), MemberRes>,
    /// The type bound to each value/fn definition.
    pub(crate) binding_types: HashMap<DefId, TypeId>,
    /// The compile-time-known `i128` value of each `const` binding that folded to
    /// a `comptime_int` (a literal, or a `+`/`-`/`*`/unary-`-` expression over
    /// such values). Drives the comptime-int overflow range check at sized-int
    /// coercion sites; absent entries are simply "not statically known here".
    pub(crate) comptime_int_values: HashMap<DefId, i128>,
    /// The evaluated type of each `const Name = <type-expr>` item, cached so a
    /// type used as a value (e.g. `ParseError` as a switch scrutinee's set) is
    /// computed once.
    pub(crate) item_types: HashMap<DefId, TypeId>,
    /// Diagnostics, in roughly source order.
    pub(crate) diags: Vec<Diagnostic>,
    /// The enclosing-function frame stack (for `return`/`try`).
    pub(crate) fn_stack: Vec<FnFrame>,
    /// The enclosing container type stack (for `@This()`).
    pub(crate) self_stack: Vec<TypeId>,
    /// Definitions known to be `const` (immutable). Both `const` and `var`
    /// locals/items are `DefKind::Local`/`DefKind::Item` in the resolver, so we
    /// track const-ness here as declarations are checked, to flag assignment to
    /// an immutable binding.
    pub(crate) const_defs: HashSet<DefId>,
    /// Recursion-depth guard.
    depth: u32,
    depth_exceeded: bool,
}

impl<'a> Checker<'a> {
    /// Builds a checker over an already-resolved file.
    pub fn new(resolved: &'a Resolved) -> Checker<'a> {
        Checker {
            resolved,
            arena: TypeArena::new(),
            types: HashMap::new(),
            members: HashMap::new(),
            binding_types: HashMap::new(),
            comptime_int_values: HashMap::new(),
            item_types: HashMap::new(),
            diags: Vec::new(),
            fn_stack: Vec::new(),
            self_stack: Vec::new(),
            const_defs: HashSet::new(),
            depth: 0,
            depth_exceeded: false,
        }
    }

    /// Type-checks a whole file and consumes the checker into a [`Typed`].
    pub fn run(mut self, file: &SourceFile) -> Typed {
        // Pre-bind every top-level item's type so forward references between
        // items type-check (a fn may call a fn declared later). Consts/vars first
        // so a fn signature that names a const type (e.g. a `const E = error{...}`
        // used as a parameter type) sees the denoted type already.
        for item in &file.items {
            if matches!(item, Item::Const { .. } | Item::Var { .. }) {
                self.predeclare_item(item);
            }
        }
        for item in &file.items {
            if matches!(item, Item::Fn { .. }) {
                self.predeclare_item(item);
            }
        }
        for item in &file.items {
            self.check_item(item);
        }
        Typed {
            arena: self.arena,
            types: self.types,
            members: self.members,
            binding_types: self.binding_types,
            diagnostics: self.diags,
        }
    }

    // =====================================================================
    //  Diagnostics, recording, depth guard
    // =====================================================================

    /// Records an error diagnostic.
    pub(crate) fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::error(span, message));
    }

    /// Records a warning diagnostic.
    pub(crate) fn warn(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::warning(span, message));
    }

    /// Records the inferred type of an occurrence.
    pub(crate) fn record(&mut self, span: Span, ty: TypeId) {
        self.types.insert((span.start, span.end), ty);
    }

    /// Records the resolved member of a previously-deferred occurrence.
    pub(crate) fn record_member(&mut self, span: Span, res: MemberRes) {
        self.members.insert((span.start, span.end), res);
    }

    /// Enters one level of recursion; returns `false` past the cap.
    pub(crate) fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            if !self.depth_exceeded {
                self.depth_exceeded = true;
                self.error(Span::default(), "expression nesting too deep to type-check");
            }
            self.depth -= 1;
            return false;
        }
        true
    }

    /// Leaves one level of recursion.
    pub(crate) fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Expression-level depth guard (shares the recursion counter).
    pub(crate) fn expr_enter(&mut self) -> bool {
        self.enter()
    }

    /// Expression-level depth-guard exit.
    pub(crate) fn expr_leave(&mut self) {
        self.leave();
    }

    /// The resolution recorded for the occurrence at `span`, if any.
    pub(crate) fn resolution_at(&self, span: Span) -> Option<Resolution> {
        self.resolved.uses.at(span).map(|u| u.res)
    }

    // =====================================================================
    //  Items
    // =====================================================================

    /// Pass A: bind a top-level item's type so forward references resolve.
    fn predeclare_item(&mut self, item: &Item) {
        match item {
            Item::Const {
                name, ty, value, ..
            } => {
                if let Some(def) = self.def_of(item.span()) {
                    let (value_ty, denoted) = self.item_value_type(name, ty.as_ref(), value);
                    self.binding_types.insert(def, value_ty);
                    if let Some(d) = denoted {
                        // A `const Name = <type-expr>` used as a type evaluates to
                        // the denoted type, not to `type`.
                        self.item_types.insert(def, d);
                    }
                }
            }
            Item::Var { ty, value, .. } => {
                if let Some(def) = self.def_of(item.span()) {
                    let t = match (ty, value) {
                        (Some(ty), _) => self.eval_type(ty),
                        (None, Some(v)) => self.synth(v),
                        (None, None) => self.arena.t_deferred(),
                    };
                    self.binding_types.insert(def, t);
                }
            }
            Item::Fn {
                params, ret, span, ..
            } => {
                if let Some(def) = self.def_of(*span) {
                    let sig = self.build_fn_sig(params, ret);
                    let fnty = self.arena.intern_fn(sig);
                    self.binding_types.insert(def, fnty);
                }
            }
            Item::Test { .. } | Item::Comptime { .. } => {}
        }
    }

    /// Computes a `const Name = value` item's `(value_type, denoted_type)`. A
    /// const bound to a type expression has *value* type `type`, and its
    /// `denoted_type` is the type it names (so `const ParseError = error{...}`
    /// used as a type evaluates to that error set). A non-type value has no
    /// denoted type; an `@import` is a module namespace.
    fn item_value_type(
        &mut self,
        name: &str,
        ty: Option<&Expr>,
        value: &Expr,
    ) -> (TypeId, Option<TypeId>) {
        if let Some(ty) = ty {
            let t = self.eval_type(ty);
            return (t, None);
        }
        if Self::is_type_expr(value) {
            let denoted = self.eval_type_named(value, name);
            return (self.arena.t_type(), Some(denoted));
        }
        if let Expr::Builtin { name: b, args, .. } = value {
            if b == "@import" {
                return (self.arena.t_deferred(), None);
            }
            // `const Self = @This()` denotes the enclosing container type; the
            // const's value-type is `type`, its denoted type is that container.
            if b == "@This" {
                let denoted = self
                    .self_stack
                    .last()
                    .copied()
                    .unwrap_or_else(|| self.arena.t_deferred());
                return (self.arena.t_type(), Some(denoted));
            }
            // `const T = @TypeOf(e)` denotes `e`'s type; its value-type is `type`.
            if b == "@TypeOf" {
                if let Some(first) = args.first() {
                    let denoted = self.synth(first);
                    let value_ty = self.synth(value);
                    return (value_ty, Some(denoted));
                }
            }
        }
        // A `const X = SomeOtherType` aliasing another named type also denotes a
        // type; carry the denoted type through for type-position uses.
        let vt = self.synth(value);
        if matches!(self.arena.get(vt), Type::TypeType) {
            if let Expr::Ident { span, .. } = value {
                if let Some(k2_resolve::Resolution::Def(id)) = self.resolution_at(*span) {
                    if let Some(&d) = self.item_types.get(&id) {
                        return (vt, Some(d));
                    }
                }
            }
        }
        (vt, None)
    }

    /// Evaluates a container/type expression, passing a display name for the
    /// nominal aggregate it may build.
    fn eval_type_named(&mut self, value: &Expr, name: &str) -> TypeId {
        if let Expr::Container(c) = value {
            self.eval_container(c, name)
        } else {
            self.eval_type(value)
        }
    }

    /// `true` if `e` is a type-constructor expression (its value is a type).
    fn is_type_expr(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Optional { .. }
                | Expr::Pointer { .. }
                | Expr::Slice { .. }
                | Expr::ArrayType { .. }
                | Expr::ErrorUnion { .. }
                | Expr::FnType { .. }
                | Expr::ErrorSet { .. }
                | Expr::AnyType { .. }
                | Expr::Container(_)
        )
    }

    /// Pass B: check an item's bodies/values.
    fn check_item(&mut self, item: &Item) {
        if !self.enter() {
            return;
        }
        match item {
            Item::Const {
                name, ty, value, ..
            } => self.check_const_like(name, ty.as_ref(), Some(value), item.span(), false, true),
            Item::Var {
                name, ty, value, ..
            } => {
                self.check_const_like(name, ty.as_ref(), value.as_ref(), item.span(), false, false)
            }
            Item::Fn {
                params,
                ret,
                body,
                name,
                ..
            } => self.check_fn(name, params, ret, body.as_deref()),
            Item::Test { body, .. } => {
                // A `test` body runs as an `!void` so `try` is allowed (spec §06).
                self.push_inferred_void_fn();
                for s in body {
                    self.check_stmt(s);
                }
                self.fn_stack.pop();
            }
            Item::Comptime { body, .. } => {
                self.push_inferred_void_fn();
                for s in body {
                    self.check_stmt(s);
                }
                self.fn_stack.pop();
            }
        }
        self.leave();
    }

    /// Pushes a synthetic `!void` frame (inferred error union of void), used for
    /// `test`/`comptime` blocks so `try` inside them is legal.
    fn push_inferred_void_fn(&mut self) {
        let void = self.arena.t_void();
        self.fn_stack.push(FnFrame {
            ret: void,
            ret_is_error_union: true,
            ret_ok: void,
            ret_err: ErrSetRef::Inferred,
            saw_value_return: false,
        });
    }

    /// Checks a `const`/`var` (item or statement): evaluate the annotation,
    /// check the initializer against it (or synthesize), and bind the name. Also
    /// records the *denoted* type for a `const Name = <type-expr>` so a later use
    /// of `Name` in a type position evaluates to the type it names.
    pub(crate) fn check_const_like(
        &mut self,
        name: &str,
        ty: Option<&Expr>,
        value: Option<&Expr>,
        decl_span: Span,
        _is_local: bool,
        is_const: bool,
    ) {
        let (bound, denoted) = match (ty, value) {
            (Some(ty), Some(v)) => {
                let t = self.eval_type(ty);
                // `const X = @import(...)` binds a module; do not re-check it.
                if Self::is_import(v) {
                    (self.arena.t_deferred(), None)
                } else {
                    self.check(v, t);
                    (t, None)
                }
            }
            (Some(ty), None) => (self.eval_type(ty), None),
            (None, Some(v)) => self.item_value_type(name, None, v),
            (None, None) => (self.arena.t_deferred(), None),
        };
        if let Some(def) = self.def_of(decl_span) {
            self.binding_types.insert(def, bound);
            if is_const {
                self.const_defs.insert(def);
            }
            if let Some(d) = denoted {
                self.item_types.insert(def, d);
            }
            // Record a `const`'s compile-time-known integer value when it folds to
            // a `comptime_int` (no sized annotation forced it). This lets a later
            // use of the binding be range-checked at a sized-int coercion site.
            if is_const && matches!(self.arena.get(bound), Type::ComptimeInt) {
                if let Some(v) = value.and_then(|v| self.fold_comptime_int(v)) {
                    self.comptime_int_values.insert(def, v);
                }
            }
        }
    }

    /// `true` if `v` is exactly an `@import("...")`.
    fn is_import(v: &Expr) -> bool {
        matches!(v, Expr::Builtin { name, .. } if name == "@import")
    }

    /// Checks a function: build the signature, bind params, push the frame, and
    /// walk the body, then verify the body returns appropriately.
    fn check_fn(&mut self, _name: &str, params: &[Param], ret: &Expr, body: Option<&[Stmt]>) {
        let sig = self.build_fn_sig(params, ret);
        let ret_ty = sig.ret;
        // Bind each parameter to its type.
        for (p, info) in params.iter().zip(sig.params.iter()) {
            if let Some(def) = self.def_of(p.span) {
                self.binding_types.insert(def, info.ty);
            }
        }
        let (is_eu, ok, err) = self.error_union_parts(ret_ty);
        self.fn_stack.push(FnFrame {
            ret: ret_ty,
            ret_is_error_union: is_eu,
            ret_ok: ok,
            ret_err: err,
            saw_value_return: false,
        });
        // The enclosing container type (for `@This()`) is the top of self_stack;
        // a free function inherits it (file scope -> no Self), which is fine.
        if let Some(body) = body {
            for s in body {
                self.check_stmt(s);
            }
            self.check_fn_fallthrough(ret_ty, ok, is_eu, ret);
        }
        self.fn_stack.pop();
    }

    /// If a non-void, non-deferred function never produced a value `return`,
    /// flag it conservatively (only when the body could fall through).
    fn check_fn_fallthrough(&mut self, ret: TypeId, ok: TypeId, is_eu: bool, ret_expr: &Expr) {
        let frame = self.fn_stack.last().expect("fn frame present");
        if frame.saw_value_return {
            return;
        }
        let effective = if is_eu { ok } else { ret };
        let t = self.arena.get(effective);
        let needs_value = !matches!(
            t,
            Type::Void | Type::NoReturn | Type::Deferred | Type::AnyType | Type::Error
        );
        if needs_value {
            self.error(
                ret_expr.span(),
                format!(
                    "function must return a value of type `{}`",
                    self.arena.fmt(effective)
                ),
            );
        }
    }

    /// Splits a return type into `(is_error_union, ok, err)`.
    fn error_union_parts(&self, ret: TypeId) -> (bool, TypeId, ErrSetRef) {
        match self.arena.get(ret) {
            Type::ErrorUnion { err, ok } => (true, *ok, *err),
            _ => (false, ret, ErrSetRef::Inferred),
        }
    }

    /// Builds a [`FnSig`] from AST params and a return-type expression.
    pub(crate) fn build_fn_sig(&mut self, params: &[Param], ret: &Expr) -> FnSig {
        let mut infos = Vec::with_capacity(params.len());
        let mut has_comptime = false;
        let mut has_anytype = false;
        for p in params {
            let ty = self.eval_type(&p.ty);
            if p.is_comptime {
                has_comptime = true;
            }
            if matches!(self.arena.get(ty), Type::AnyType) {
                has_anytype = true;
            }
            infos.push(ParamInfo {
                name: p.name.clone(),
                ty,
                is_comptime: p.is_comptime,
                span: p.span,
            });
        }
        let ret_ty = self.eval_type(ret);
        FnSig {
            params: infos,
            is_varargs: false,
            ret: ret_ty,
            has_comptime_param: has_comptime,
            has_anytype_param: has_anytype,
        }
    }

    /// The definition (binding site) declared at `span`, looked up by walking the
    /// resolver's def table. The resolver keys a binding's `Def.span` to the
    /// declaration span we pass here.
    pub(crate) fn def_of(&self, span: Span) -> Option<DefId> {
        self.resolved
            .defs
            .iter()
            .find(|d| {
                d.span.start == span.start
                    && d.span.end == span.end
                    && !matches!(d.kind, DefKind::Predeclared)
            })
            .map(|d| d.id)
    }

    // =====================================================================
    //  Statements
    // =====================================================================

    /// Checks one statement.
    pub(crate) fn check_stmt(&mut self, stmt: &Stmt) {
        if !self.enter() {
            return;
        }
        match stmt {
            Stmt::Const {
                name,
                ty,
                value,
                span,
            } => self.check_const_like(name, ty.as_ref(), Some(value), *span, true, true),
            Stmt::Var {
                name,
                ty,
                value,
                span,
            } => self.check_const_like(name, ty.as_ref(), value.as_ref(), *span, true, false),
            Stmt::Defer { body, .. } => self.check_stmt(body),
            Stmt::Errdefer { capture, body, .. } => {
                // `errdefer |e|` binds the enclosing fn's error set to `e`. The
                // resolver records the capture's `Def` at `body.span()`.
                if capture.is_some() {
                    let err_ty = self.current_err_set_type();
                    if let Some(def) = self.def_of(body.span()) {
                        self.binding_types.insert(def, err_ty);
                    }
                }
                self.check_stmt(body);
            }
            Stmt::Return { value, span } => self.check_return(value.as_ref(), *span),
            Stmt::Expr { expr, .. } => self.check_expr_stmt(expr),
            Stmt::Assign {
                target, op, value, ..
            } => self.check_assign(target, *op, value),
            Stmt::Comptime { body, .. } => {
                for s in body {
                    self.check_stmt(s);
                }
            }
            Stmt::Block { body, .. } => {
                for s in body {
                    self.check_stmt(s);
                }
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => {
                // Statement context: result discarded, no `else` required.
                let void = self.arena.t_void();
                self.check(expr, void);
            }
            Stmt::Break { value, .. } => {
                if let Some(v) = value {
                    self.synth(v);
                }
            }
            Stmt::Continue { .. } => {}
        }
        self.leave();
    }

    /// Checks a bare expression statement. An *unhandled* concrete error union
    /// used for its effect is an error (spec §06.2: use `try`/`catch`/`_ =`).
    fn check_expr_stmt(&mut self, expr: &Expr) {
        let t = self.synth(expr);
        if let Type::ErrorUnion { .. } = self.arena.get(t) {
            self.error(
                expr.span(),
                "error union must be handled with `try`, `catch`, or `_ =`",
            );
        }
    }

    /// Checks a `return [value];` against the enclosing function's return type.
    fn check_return(&mut self, value: Option<&Expr>, span: Span) {
        let Some(frame) = self.fn_stack.last() else {
            // A return outside any function (should not happen post-resolve).
            if let Some(v) = value {
                self.synth(v);
            }
            return;
        };
        let ret = frame.ret;
        let ok = frame.ret_ok;
        let is_eu = frame.ret_is_error_union;
        match value {
            None => {
                // `return;` requires void (or `!void`).
                let ok_is_void = matches!(self.arena.get(ok), Type::Void);
                let ret_is_void = matches!(self.arena.get(ret), Type::Void);
                if !(ret_is_void || (is_eu && ok_is_void)) {
                    self.error(
                        span,
                        format!(
                            "function returns `{}` but `return;` has no value",
                            self.arena.fmt(ret)
                        ),
                    );
                }
            }
            Some(v) => {
                if matches!(self.arena.get(ret), Type::Void) {
                    // A void function must not return a value (unless deferred).
                    let vt = self.synth(v);
                    if !self.arena.is_bottom(vt) {
                        self.error(v.span(), "void function cannot return a value");
                    }
                } else {
                    self.check(v, ret);
                    if let Some(frame) = self.fn_stack.last_mut() {
                        frame.saw_value_return = true;
                    }
                }
            }
        }
    }

    /// Checks an assignment statement.
    fn check_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr) {
        // The discard sink accepts anything.
        if let Expr::Ident { name, .. } = target {
            if name == "_" {
                self.synth(value);
                return;
            }
        }
        // Immutability: assigning to a `const`/param binding is illegal.
        if let Expr::Ident { name, span } = target {
            if let Some(Resolution::Def(id)) = self.resolution_at(*span) {
                let kind = self.resolved.defs[id.index()].kind;
                let immutable = matches!(kind, DefKind::Param)
                    || (matches!(kind, DefKind::Local | DefKind::Item)
                        && self.is_const_binding(id));
                if immutable {
                    self.error(
                        *span,
                        format!("cannot assign to immutable binding `{name}`"),
                    );
                }
            }
        }
        // Lvalue mutability through a pointer/slice: writing through a `*const T`
        // or to an element of a `[]const T` (or `*const [N]T`) is illegal, even
        // though the place itself synthesizes to a writable value type.
        self.check_lvalue_mutable(target);
        let target_ty = self.synth(target);
        if matches!(op, AssignOp::Eq) {
            self.check(value, target_ty);
        } else {
            // Compound assignment (`+=`, …): the target must be numeric AND the
            // value must coerce to it — the plain `=` path already checks the
            // value, so the compound path must too (no soundness asymmetry).
            let vt = self.synth(value);
            if !self.arena.is_bottom(target_ty) && !self.arena.is_bottom(vt) {
                if !self.numeric(target_ty) {
                    self.error(
                        target.span(),
                        format!(
                            "compound assignment requires a numeric target, found `{}`",
                            self.arena.fmt(target_ty)
                        ),
                    );
                } else if !self.numeric(vt) || self.try_unify_numeric(target_ty, vt).is_none() {
                    self.error(
                        value.span(),
                        format!(
                            "expected `{}`, found `{}`",
                            self.arena.fmt(target_ty),
                            self.arena.fmt(vt)
                        ),
                    );
                }
            }
        }
    }

    /// Reports an error if `target` is an assignment place that writes through a
    /// read-only borrow: `(*const T).*`, an index of a `[]const T` (or of a
    /// `*const [N]T`), or a field reached through a `*const` base. Writes through
    /// `*T` / `[]T` and to a plain `var` are allowed.
    fn check_lvalue_mutable(&mut self, target: &Expr) {
        match target {
            // `p.* = ...` — illegal when `p: *const T`.
            Expr::Deref { base, span } => {
                let bt = self.synth(base);
                if let Type::Pointer { is_const: true, .. } = self.arena.get(bt) {
                    self.error(*span, "cannot assign through a `*const` pointer");
                }
            }
            // `s[i] = ...` — illegal when `s: []const T` or `s: *const [N]T`.
            Expr::Index { base, span, .. } => {
                let bt = self.synth(base);
                if self.is_const_indexable(bt) {
                    self.error(*span, "cannot assign to an element of a `const` slice");
                }
            }
            // `p.field = ...` — illegal when `p` is a `*const` pointer that is
            // auto-dereferenced to reach the field's aggregate.
            Expr::Field { base, span, .. } => {
                let bt = self.synth(base);
                if let Type::Pointer { is_const: true, .. } = self.arena.get(bt) {
                    self.error(*span, "cannot assign through a `*const` pointer");
                }
            }
            _ => {}
        }
    }

    /// `true` if writing to an element through `base` mutates read-only storage:
    /// a `[]const T` slice, or a `*const [N]T` pointer-to-array.
    fn is_const_indexable(&self, base: TypeId) -> bool {
        match self.arena.get(base) {
            Type::Slice { is_const, .. } => *is_const,
            Type::Pointer {
                is_const, pointee, ..
            } => *is_const && matches!(self.arena.get(*pointee), Type::Array { .. }),
            _ => false,
        }
    }

    /// `true` if `id` denotes a `const` binding (not a `var`). Both `const` and
    /// `var` locals/items are `DefKind::Local`/`DefKind::Item` in the resolver,
    /// so const-ness is tracked in [`Self::const_defs`] as declarations are
    /// checked.
    fn is_const_binding(&self, id: DefId) -> bool {
        self.const_defs.contains(&id)
    }

    /// The error-set *type* of the enclosing function (for `catch`/`errdefer`
    /// captures): the function's error set as an `ErrorSet`/`anyerror` type.
    pub(crate) fn current_err_set_type(&mut self) -> TypeId {
        match self.fn_stack.last() {
            Some(frame) => match frame.ret_err {
                ErrSetRef::Set(e) => self.arena.intern(Type::ErrorSet(e)),
                ErrSetRef::Any => self.arena.t_anyerror(),
                ErrSetRef::Inferred | ErrSetRef::Deferred => self.arena.t_anyerror(),
            },
            None => self.arena.t_anyerror(),
        }
    }

    // =====================================================================
    //  Container evaluation (struct/enum/union -> nominal Info)
    // =====================================================================

    /// Builds the nominal type for a container literal, recording its fields and
    /// nested members. The result is interned by the container's span.
    pub(crate) fn eval_container(&mut self, c: &Container, name: &str) -> TypeId {
        // Push a Self placeholder so a recursive `@This()` inside resolves. We
        // intern a forward struct/enum/union shell first only for structs that
        // reference Self; for simplicity we evaluate fields with the current
        // self_stack, then push the final type for nested decls.
        match &c.kind {
            ContainerKind::Struct { is_extern } => self.eval_struct(c, name, *is_extern),
            ContainerKind::Enum { tag } => self.eval_enum(c, name, tag.as_deref()),
            ContainerKind::Union { tag } => self.eval_union(c, name, tag),
        }
    }

    /// Evaluates a `struct {...}` body into a [`Type::Struct`].
    fn eval_struct(&mut self, c: &Container, name: &str, is_extern: bool) -> TypeId {
        let mut fields = Vec::new();
        for m in &c.members {
            if let Member::Field(f) = m {
                let ty = match &f.ty {
                    Some(t) => self.eval_type(t),
                    None => self.arena.t_deferred(),
                };
                fields.push(FieldInfo {
                    name: f.name.clone(),
                    ty,
                    has_default: f.default.is_some(),
                    is_comptime: f.is_comptime,
                    span: f.span,
                });
            }
        }
        let info = StructInfo {
            def: self.def_of(c.span),
            name: name.to_string(),
            span: c.span,
            is_extern,
            fields,
            decls: Vec::new(),
        };
        let ty = self.arena.intern_struct(info);
        self.eval_container_decls(c, ty);
        ty
    }

    /// Evaluates an `enum {...}` body into a [`Type::Enum`].
    fn eval_enum(&mut self, c: &Container, name: &str, tag: Option<&Expr>) -> TypeId {
        let mut variants = Vec::new();
        for m in &c.members {
            if let Member::Field(f) = m {
                variants.push(EnumVariant {
                    name: f.name.clone(),
                    span: f.span,
                });
            }
        }
        let tag_ty = match tag {
            Some(t) => self.eval_type(t),
            None => self.arena.t_usize(), // inferred backing; width unused in v0.5.
        };
        let info = EnumInfo {
            def: self.def_of(c.span),
            name: name.to_string(),
            span: c.span,
            tag: tag_ty,
            variants,
            decls: Vec::new(),
        };
        let ty = self.arena.intern_enum(info);
        self.eval_container_decls(c, ty);
        ty
    }

    /// Evaluates a `union {...}` body into a [`Type::Union`].
    fn eval_union(&mut self, c: &Container, name: &str, tag: &UnionTag) -> TypeId {
        let mut variants = Vec::new();
        for m in &c.members {
            if let Member::Field(f) = m {
                let payload = match &f.ty {
                    Some(t) => self.eval_type(t),
                    None => self.arena.t_void(),
                };
                variants.push(UnionVariant {
                    name: f.name.clone(),
                    payload,
                    span: f.span,
                });
            }
        }
        let kind = match tag {
            UnionTag::None => UnionTagKind::None,
            UnionTag::Inferred => UnionTagKind::Inferred,
            UnionTag::Typed(_) => UnionTagKind::Typed,
        };
        let info = UnionInfo {
            def: self.def_of(c.span),
            name: name.to_string(),
            span: c.span,
            tag: kind,
            variants,
            decls: Vec::new(),
        };
        let ty = self.arena.intern_union(info);
        self.eval_container_decls(c, ty);
        ty
    }

    /// Walks a container's nested const/var/fn members: records each member's
    /// type into the nominal Info and checks its body, with the container type
    /// on the `self_stack` (for `@This()` and `self` typing).
    fn eval_container_decls(&mut self, c: &Container, container_ty: TypeId) {
        self.self_stack.push(container_ty);
        let mut decls: Vec<MemberDecl> = Vec::new();
        for m in &c.members {
            if let Member::Decl(item) = m {
                match item {
                    Item::Fn {
                        name,
                        params,
                        ret,
                        is_pub,
                        span,
                        ..
                    } => {
                        let sig = self.build_fn_sig(params, ret);
                        let fnty = self.arena.intern_fn(sig);
                        if let Some(def) = self.def_of(*span) {
                            self.binding_types.insert(def, fnty);
                            decls.push(MemberDecl {
                                name: name.clone(),
                                is_pub: *is_pub,
                                def,
                                ty: fnty,
                            });
                        }
                    }
                    Item::Const {
                        name,
                        ty,
                        value,
                        is_pub,
                        span,
                        ..
                    } => {
                        let (t, denoted) = self.item_value_type(name, ty.as_ref(), value);
                        if let Some(def) = self.def_of(*span) {
                            self.binding_types.insert(def, t);
                            if let Some(d) = denoted {
                                self.item_types.insert(def, d);
                            }
                            decls.push(MemberDecl {
                                name: name.clone(),
                                is_pub: *is_pub,
                                def,
                                ty: t,
                            });
                        }
                    }
                    Item::Var {
                        name,
                        ty,
                        value,
                        is_pub,
                        span,
                        ..
                    } => {
                        let t = match (ty, value) {
                            (Some(ty), _) => self.eval_type(ty),
                            (None, Some(v)) => self.synth(v),
                            (None, None) => self.arena.t_deferred(),
                        };
                        if let Some(def) = self.def_of(*span) {
                            self.binding_types.insert(def, t);
                            decls.push(MemberDecl {
                                name: name.clone(),
                                is_pub: *is_pub,
                                def,
                                ty: t,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        // Store the decls into the nominal Info now that we have them all.
        self.store_decls(container_ty, decls);
        // Pass B: check the member bodies (now that all member types exist).
        for m in &c.members {
            if let Member::Decl(item) = m {
                self.check_item(item);
            }
        }
        self.self_stack.pop();
    }

    /// Records the collected member declarations into a nominal Info.
    fn store_decls(&mut self, container_ty: TypeId, decls: Vec<MemberDecl>) {
        match self.arena.get(container_ty).clone() {
            Type::Struct(id) => self.arena.structs[id.0 as usize].decls = decls,
            Type::Enum(id) => self.arena.enums[id.0 as usize].decls = decls,
            Type::Union(id) => self.arena.unions[id.0 as usize].decls = decls,
            _ => {}
        }
    }

    // =====================================================================
    //  Capture binding for if/while/switch/for
    // =====================================================================

    /// Binds the names of a `|a, b|` capture (by looking up each name's `Def` via
    /// its span) to the given type(s). For optional/error-union/payload captures
    /// the single name binds to `payload`.
    pub(crate) fn bind_capture(&mut self, capture: Option<&Capture>, payload: TypeId) {
        if let Some(c) = capture {
            for n in &c.names {
                if n.name == "_" {
                    continue;
                }
                let ty = if n.by_ref {
                    self.arena.ptr(false, payload)
                } else {
                    payload
                };
                if let Some(def) = self.def_of(n.span) {
                    self.binding_types.insert(def, ty);
                }
            }
        }
    }

    // =====================================================================
    //  for-operands (iteration element typing)
    // =====================================================================

    /// Binds a `for` capture name list against the operands' element types.
    pub(crate) fn bind_for_captures(
        &mut self,
        operands: &[ForOperand],
        captures: &[k2_syntax::CaptureName],
    ) {
        for (i, cap) in captures.iter().enumerate() {
            let elem = operands
                .get(i)
                .map(|op| self.for_operand_elem(op))
                .unwrap_or_else(|| self.arena.t_deferred());
            if cap.name == "_" {
                continue;
            }
            let ty = if cap.by_ref {
                self.arena.ptr(false, elem)
            } else {
                elem
            };
            if let Some(def) = self.def_of(cap.span) {
                self.binding_types.insert(def, ty);
            }
        }
    }

    /// The element type produced by iterating one `for` operand.
    fn for_operand_elem(&mut self, op: &ForOperand) -> TypeId {
        match op {
            ForOperand::Value(e) => {
                let t = self.synth(e);
                match self.arena.get(t) {
                    Type::Slice { elem, .. } | Type::Array { elem, .. } => *elem,
                    Type::Pointer { pointee, .. } => match self.arena.get(*pointee).clone() {
                        Type::Array { elem, .. } => elem,
                        _ => self.arena.t_deferred(),
                    },
                    _ => self.arena.t_deferred(),
                }
            }
            // An index range `0..` yields `usize` counters.
            ForOperand::Range { lo, hi, .. } => {
                let usize_t = self.arena.t_usize();
                self.check(lo, usize_t);
                if let Some(hi) = hi {
                    self.check(hi, usize_t);
                }
                usize_t
            }
        }
    }
}
