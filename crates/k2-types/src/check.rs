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
    struct_layout_of, EnumInfo, EnumVariant, ErrSetRef, FieldInfo, FnSig, IntBits, MemberDecl,
    MemberRes, ParamInfo, StructInfo, StructLayout, Type, TypeId, UnionInfo, UnionTagKind,
    UnionVariant,
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

    // ---- comptime engine state (v0.6) ----------------------------------
    /// The remaining comptime fuel for the in-progress evaluation. Reset per
    /// top-level boundary; decremented on every evaluation step / loop back-edge
    /// / call, guaranteeing termination.
    pub(crate) comptime_fuel: u64,
    /// Whether the fuel-exhaustion diagnostic has already been emitted for the
    /// current evaluation (so it is reported once, not once per step).
    pub(crate) comptime_fuel_reported: bool,
    /// The fully comptime-known [`Value`] of a binding, where the engine folded
    /// it (a `@typeInfo` const, an `inline for` loop var, a `@sizeOf` result).
    pub(crate) comptime_const_values: HashMap<DefId, crate::value::Value>,
    /// The comptime-known integer value at an expression span (e.g. a `@sizeOf`
    /// or `serializedSize(T)` occurrence), used to resolve a comptime array
    /// length to a concrete count.
    pub(crate) comptime_span_ints: HashMap<(u32, u32), i128>,
    /// Index from a fn's [`DefId`] to its AST item, built once in [`Self::run`],
    /// so the generic-instantiation engine can fetch a body without re-walking.
    pub(crate) fn_items: HashMap<DefId, k2_syntax::Item>,
    /// The generic-instantiation cache: `(fn, arg tuple) -> result`.
    pub(crate) inst_cache: crate::generic::InstCache,
    /// The in-progress instantiation stack (recursion guard + error context).
    pub(crate) inst_stack: Vec<(DefId, Span)>,
    /// The synthesized reflection descriptor types (`TypeInfo`, `StructField`, …).
    pub(crate) reflect: crate::reflect::ReflectTypes,
    /// Content-keyed cache for `@Type`-of-struct, so a round-trip
    /// `@Type(@typeInfo(S))` yields one stable nominal type.
    pub(crate) reify_struct_cache: crate::reflect::ReifyStructCache,
    /// A monotonically increasing counter for fresh synthetic spans, used so
    /// each distinct generic instantiation / `@Type` struct interns a distinct
    /// nominal type.
    pub(crate) synthetic_span_counter: u32,
    /// Spans whose expression *value is a `type`* (a type-returning generic call
    /// `List(u32)`), mapping to the denoted aggregate. An access `List(u32).init`
    /// against such a base is an *associated* call (no implicit receiver), not
    /// method-call sugar.
    pub(crate) type_valued_spans: HashMap<(u32, u32), TypeId>,
    /// Nesting depth inside a *statically-conditional* branch (an `if`/`while`/
    /// `for`/`switch` arm). A statement-position `@compileError`/`@panic` is only
    /// fired eagerly at conditional depth 0 (an unconditionally-reached fn-body
    /// statement); inside a branch the engine's own live-branch evaluation
    /// decides, so a dead-branch `@compileError` stays silent (spec §07.9.1).
    pub(crate) cond_depth: u32,
    /// Instantiated container types whose method bodies have already been
    /// re-type-checked, keyed by the instantiation's nominal [`TypeId`] (unique
    /// per distinct argument tuple). Guarantees each distinct instantiation's
    /// bodies are checked exactly once (spec §07.4: per-instantiation soundness),
    /// not per use site.
    pub(crate) rechecked_insts: HashSet<TypeId>,
    /// Per-instantiation member resolutions, keyed by `(enclosing instantiated
    /// struct TypeId, member occurrence span)`. The plain [`members`] table is
    /// keyed by span ALONE, so it cannot distinguish a member resolved differently
    /// across two instantiations of the same generic — e.g. `Context.lessThan`
    /// inside `Sorter(T, Asc)` vs `Sorter(T, Desc)`, where `Context` is a comptime
    /// TYPE param dispatched through a member. While re-checking an instantiation's
    /// method bodies (see [`crate::generic::Checker::recheck_instantiated_methods`])
    /// the current struct type is pushed onto [`inst_member_ctx`], and every member
    /// resolution recorded there is ALSO stored here under that struct type, so the
    /// MIR lowerer can recover the per-instantiation target (the span-keyed table
    /// holds whichever instantiation ran last, which is order-dependent garbage for
    /// the multi-context case).
    pub(crate) inst_members: HashMap<(TypeId, (u32, u32)), MemberRes>,
    /// The stack of enclosing instantiated struct types currently being
    /// re-type-checked, innermost last. Non-empty exactly while inside
    /// `recheck_instantiated_methods`; its top is the key under which member
    /// resolutions are mirrored into [`inst_members`].
    pub(crate) inst_member_ctx: Vec<TypeId>,
    /// The C-interop linkage recorded for each `extern`/`export` function during
    /// checking (v0.19), moved into [`Typed::extern_fns`] at the end of [`run`].
    pub(crate) extern_fns: HashMap<DefId, crate::ty::ExternInfo>,
}

impl<'a> Checker<'a> {
    /// Builds a checker over an already-resolved file.
    pub fn new(resolved: &'a Resolved) -> Checker<'a> {
        let mut arena = TypeArena::new();
        let reflect = Checker::install_reflection_types(&mut arena);
        Checker {
            resolved,
            arena,
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
            comptime_fuel: crate::comptime::COMPTIME_FUEL,
            comptime_fuel_reported: false,
            comptime_const_values: HashMap::new(),
            comptime_span_ints: HashMap::new(),
            fn_items: HashMap::new(),
            inst_cache: HashMap::new(),
            inst_stack: Vec::new(),
            reflect,
            reify_struct_cache: HashMap::new(),
            synthetic_span_counter: 0,
            type_valued_spans: HashMap::new(),
            cond_depth: 0,
            rechecked_insts: HashSet::new(),
            inst_members: HashMap::new(),
            inst_member_ctx: Vec::new(),
            extern_fns: HashMap::new(),
        }
    }

    /// Allocates a fresh synthetic span in the compiler-reserved high range, so
    /// each distinct generic instantiation / `@Type` struct interns a distinct
    /// nominal type (the arena keys structs/enums/unions by span).
    pub(crate) fn fresh_synthetic_span(&mut self) -> Span {
        // Reserve the high half of the span space for synthesized nodes; real
        // source spans live below it. The counter steps by 2 so start != end.
        let base = 0xC000_0000u32;
        let off = base + self.synthetic_span_counter.wrapping_mul(2);
        self.synthetic_span_counter = self.synthetic_span_counter.wrapping_add(1);
        Span::new(off, off + 1, 0, 0)
    }

    /// Type-checks a whole file and consumes the checker into a [`Typed`].
    pub fn run(mut self, file: &SourceFile) -> Typed {
        // Index every `fn` item (top-level and nested) by its DefId so the
        // comptime engine can fetch a generic body for instantiation.
        for item in &file.items {
            self.index_fn_items(item);
        }
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
            inst_members: self.inst_members,
            binding_types: self.binding_types,
            type_valued_spans: self.type_valued_spans,
            comptime_span_ints: self.comptime_span_ints,
            comptime_int_values: self.comptime_int_values,
            extern_fns: self.extern_fns,
            diagnostics: self.diags,
        }
    }

    /// Recursively indexes `fn` items (and fns nested in container values) by
    /// their [`DefId`], so generic instantiation can fetch a body. Fns inside a
    /// generic's returned `struct {...}` are not indexed here (they live in the
    /// instantiated nominal type), but the top-level generic functions are.
    fn index_fn_items(&mut self, item: &Item) {
        match item {
            Item::Fn { span, .. } => {
                if let Some(def) = self.def_of(*span) {
                    self.fn_items.insert(def, item.clone());
                }
            }
            Item::Const {
                value: Expr::Container(c),
                ..
            } => {
                for m in &c.members {
                    if let Member::Decl(inner) = m {
                        self.index_fn_items(inner);
                    }
                }
            }
            _ => {}
        }
    }

    // =====================================================================
    //  Diagnostics, recording, depth guard
    // =====================================================================

    /// Records an error diagnostic.
    pub(crate) fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::error(span, message));
    }

    /// Records a fully-built (labels/notes/help-bearing) error diagnostic.
    pub(crate) fn error_rich(&mut self, diag: Diagnostic) {
        self.diags.push(diag);
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
    ///
    /// A method-body span is SHARED across every instantiation of a generic and
    /// the generic's own static (`T = type`) body check. The static check resolves
    /// a member on a still-comptime type param (`Hctx.hash`, where `Hctx: type`) to
    /// [`MemberRes::Deferred`], while an instantiation (`Hctx = SomeCtx`) resolves
    /// the SAME span to a concrete [`MemberRes::Decl`]. Whichever check runs LAST
    /// would otherwise win, making member resolution depend on declaration order
    /// (a generic defined after its first use site would have its concrete member
    /// clobbered back to `Deferred` by the later static check — which the MIR then
    /// mis-lowers to an `@TypeParam` builtin intrinsic). To make this
    /// order-independent, a concrete resolution is NEVER downgraded to `Deferred`:
    /// once a span resolves to a real target, that target stands.
    pub(crate) fn record_member(&mut self, span: Span, res: MemberRes) {
        let key = (span.start, span.end);
        // Mirror a per-instantiation resolution under the enclosing instantiated
        // struct type, so the MIR can recover the right target for THIS
        // instantiation even when a sibling one resolves the same span elsewhere.
        if let Some(&struct_ty) = self.inst_member_ctx.last() {
            if res != MemberRes::Deferred {
                self.inst_members.insert((struct_ty, key), res);
            }
        }
        if res == MemberRes::Deferred {
            if let Some(prev) = self.members.get(&key) {
                if *prev != MemberRes::Deferred {
                    // Keep the already-resolved concrete target (an instantiation
                    // recorded it); do not clobber it with a deferred placeholder.
                    return;
                }
            }
        }
        self.members.insert(key, res);
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
                params,
                ret,
                span,
                is_varargs,
                ..
            } => {
                if let Some(def) = self.def_of(*span) {
                    let mut sig = self.build_fn_sig(params, ret);
                    // v0.19: carry the `...`-varargs flag so a printf-class extern
                    // call type-checks with extra trailing arguments.
                    sig.is_varargs = *is_varargs;
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
                | Expr::ManyPtr { .. }
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
                is_extern,
                is_export,
                is_varargs,
                span,
                ..
            } => self.check_fn(
                name,
                params,
                ret,
                body.as_deref(),
                *is_extern,
                *is_export,
                *is_varargs,
                *span,
            ),
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
                // A top-level forced `comptime { ... }` block is EXECUTED by the
                // engine, so a reached `@compileError`/`@panic`, an infinite loop
                // (fuel), or a div-by-zero fires here (spec §07.3.2).
                self.run_comptime_block(body);
            }
        }
        self.leave();
    }

    /// Executes a forced `comptime { ... }` block through the comptime engine so
    /// any reached `@compileError`/`@panic`/overflow/fuel-exhaustion is reported.
    /// Pure deferrals (`Diverge::NotComptime`) are silent: a forced block that
    /// merely touches a runtime-only value is left to the ordinary checker.
    fn run_comptime_block(&mut self, body: &[Stmt]) {
        self.reset_fuel();
        let mut env = crate::comptime::Env::new();
        for s in body {
            match self.eval_stmt(&mut env, s) {
                Ok(()) => {}
                // A reached diagnostic was already queued by the engine.
                Err(_) => break,
            }
        }
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
            // v0.6: fold a comptime const value (a `@typeInfo`/`@Type`/`@sizeOf`
            // result, an `inline for` loop var's field type, a generic-call
            // result) so later member access / type uses are concrete instead of
            // Deferred. Only attempt this where the bound is itself deferred or a
            // reflection/comptime type, to avoid re-evaluating already-concrete
            // initializers. The engine emits a diagnostic only for a genuinely
            // executed `@compileError`/fuel/div-by-zero, never for a deferral.
            if is_const {
                if let Some(v) = value {
                    if Self::is_comptime_value_expr(v) {
                        if let Some(cv) = self.comptime_eval_value(v) {
                            if let Some(t) = cv.as_type() {
                                self.item_types.insert(def, t);
                            }
                            self.comptime_const_values.insert(def, cv);
                        }
                    }
                }
            }
        }
    }

    /// `true` if `v` is an expression worth attempting comptime folding for in a
    /// `const` binding: a reflection/type builtin, a `comptime`-forced
    /// expression, or a generic/ordinary call (whose result may be a concrete
    /// type or comptime value).
    fn is_comptime_value_expr(v: &Expr) -> bool {
        match v {
            Expr::Builtin { name, .. } => matches!(
                name.as_str(),
                "@typeInfo"
                    | "@Type"
                    | "@field"
                    | "@hasField"
                    | "@sizeOf"
                    | "@alignOf"
                    | "@bitSizeOf"
                    | "@typeName"
                    | "@TypeOf"
                    | "@This"
                    | "@Vector"
                    // A top-level `const x = @compileError("m");` is an
                    // unconditionally-reached position, so the engine must run it
                    // and fire the message (spec §07.9.1) — a dead-branch
                    // `@compileError` still stays silent because the engine only
                    // evaluates the live branch.
                    | "@compileError"
                    | "@panic"
            ),
            Expr::Comptime { .. } => true,
            Expr::Call { .. } => true,
            Expr::Field { .. } => true,
            // Arithmetic/logic over comptime-known operands (e.g.
            // `const n = 1 << 10;`) so the binding's value is comptime-known and
            // can size an array. A runtime operand simply yields NotComptime.
            Expr::Binary { .. } | Expr::Unary { .. } | Expr::Index { .. } => true,
            _ => false,
        }
    }

    /// `true` if `v` is exactly an `@import("...")`.
    fn is_import(v: &Expr) -> bool {
        matches!(v, Expr::Builtin { name, .. } if name == "@import")
    }

    /// Checks a function: build the signature, bind params, push the frame, and
    /// walk the body, then verify the body returns appropriately. The `extern`/
    /// `export` flags drive the v0.19 C-interop checks (FFI-representability of the
    /// signature, body presence rules) and record the function's C linkage.
    #[allow(clippy::too_many_arguments)]
    fn check_fn(
        &mut self,
        name: &str,
        params: &[Param],
        ret: &Expr,
        body: Option<&[Stmt]>,
        is_extern: bool,
        is_export: bool,
        is_varargs: bool,
        fn_span: Span,
    ) {
        let sig = self.build_fn_sig(params, ret);
        let ret_ty = sig.ret;
        // ---- v0.19 C-interop checks for extern/export functions. ----
        if is_extern || is_export {
            self.check_ffi_fn(
                name, params, &sig, body, is_extern, is_export, is_varargs, fn_span,
            );
        }
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

    /// The v0.19 C-interop checks for an `extern`/`export` function: body-presence
    /// rules and FFI-representability of every parameter + the return type. On
    /// success the function's C linkage is recorded in [`Self::extern_fns`] keyed by
    /// its [`DefId`] so the MIR lowerer can emit the right symbol (undefined for
    /// `extern`, defined-global for `export`).
    #[allow(clippy::too_many_arguments)]
    fn check_ffi_fn(
        &mut self,
        name: &str,
        params: &[Param],
        sig: &FnSig,
        body: Option<&[Stmt]>,
        is_extern: bool,
        is_export: bool,
        is_varargs: bool,
        fn_span: Span,
    ) {
        // Body-presence rules: an `extern` fn declares a C symbol and must NOT have
        // a body; an `export` fn exposes a k2 body and MUST have one.
        if is_extern && body.is_some() {
            self.error(
                fn_span,
                "an `extern` function declares a C symbol and must not have a body",
            );
            return;
        }
        if is_export && body.is_none() {
            self.error(
                fn_span,
                "an `export` function must have a body to expose to C",
            );
            return;
        }
        // FFI-representability of the signature. A varargs `extern` (printf-class)
        // is allowed; its fixed params must still be representable.
        let mut representable = true;
        for p in &sig.params {
            if !self.is_ffi_representable(p.ty) {
                representable = false;
                self.error(
                    p.span,
                    format!(
                        "parameter type `{}` is not C-ABI representable in an \
                         `{}` function",
                        self.arena.fmt(p.ty),
                        if is_extern { "extern" } else { "export" }
                    ),
                );
            }
        }
        // The return type must be representable or `void`.
        if !matches!(self.arena.get(sig.ret), Type::Void) && !self.is_ffi_representable(sig.ret) {
            representable = false;
            self.error(
                fn_span,
                format!(
                    "return type `{}` is not C-ABI representable in an `{}` function",
                    self.arena.fmt(sig.ret),
                    if is_extern { "extern" } else { "export" }
                ),
            );
        }
        // Suppress params we never bound (already-flagged) from cascading.
        let _ = params;
        if !representable {
            return;
        }
        if let Some(def) = self.def_of(fn_span) {
            let kind = if is_extern {
                crate::ty::ExternKind::Extern
            } else {
                crate::ty::ExternKind::Export
            };
            self.extern_fns.insert(
                def,
                crate::ty::ExternInfo {
                    kind,
                    abi_name: name.to_string(),
                    varargs: is_varargs,
                },
            );
        }
    }

    /// `true` if `ty` is representable in the C ABI (so it may appear in an
    /// `extern`/`export` signature): a fixed-width integer, `bool`, an `f32`/`f64`,
    /// a single/many pointer, or a `void`. A k2-only type — a slice (a fat
    /// `{ptr,len}` aggregate), an optional, an error union, a non-`extern` struct
    /// by value, an 80-bit `c_longdouble` (modelled as `f128`), or a deferred type
    /// — is NOT, so the FFI gate rejects it rather than miscompiling the ABI.
    fn is_ffi_representable(&self, ty: TypeId) -> bool {
        match self.arena.get(ty) {
            // Fixed-width / pointer-width integers (incl. the `c_*` aliases, which
            // are concrete ints) and `bool` are single-register scalars.
            Type::Int { .. } | Type::Bool => true,
            // `f32`/`f64` ride an SSE register; `f128` (our `c_longdouble`) is the
            // 80-bit x87 long double, which the backend does not model — reject it.
            Type::Float { bits } => *bits == 32 || *bits == 64,
            // Single / many pointers are a raw eightbyte (a C `T *` / `const char *`).
            Type::Pointer { .. } => true,
            // An `extern struct` of representable fields lays out per the C ABI.
            Type::Struct(id) => {
                let info = &self.arena.structs[id.0 as usize];
                info.is_extern() && info.fields.iter().all(|f| self.is_ffi_representable(f.ty))
            }
            _ => false,
        }
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

    /// Pushes a fn frame for a (re-)checked body with the given return parts.
    pub(crate) fn push_fn_frame(&mut self, ret: TypeId, is_eu: bool, ok: TypeId, err: ErrSetRef) {
        self.fn_stack.push(FnFrame {
            ret,
            ret_is_error_union: is_eu,
            ret_ok: ok,
            ret_err: err,
            saw_value_return: false,
        });
    }

    /// Pops the current fn frame (the partner of [`Self::push_fn_frame`]).
    pub(crate) fn pop_fn_frame(&mut self) {
        self.fn_stack.pop();
    }

    /// Splits a return type into `(is_error_union, ok, err)`.
    pub(crate) fn error_union_parts(&self, ret: TypeId) -> (bool, TypeId, ErrSetRef) {
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
                // Execute the forced comptime block (fires reached diagnostics).
                self.run_comptime_block(body);
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
    ///
    /// A statement-position `@compileError("m");` / `@panic("m");` is in a
    /// normally-reached fn body, so the comptime engine is run for it and the
    /// message fires (spec §07.9.1). The synth path alone only yields `noreturn`
    /// without executing, so a dedicated run is needed here. (A `@compileError`
    /// guarded by an untaken `if` is not an expression statement of this shape,
    /// and the engine evaluates only live branches, so dead branches stay silent.)
    fn check_expr_stmt(&mut self, expr: &Expr) {
        if self.cond_depth == 0 && Self::is_diverging_builtin_stmt(expr) {
            self.reset_fuel();
            let mut env = crate::comptime::Env::new();
            // A queued diagnostic (the verbatim message / `@panic`) is all we
            // need; a non-comptime operand simply defers silently.
            let _ = self.eval_expr(&mut env, expr);
        }
        let t = self.synth(expr);
        if let Type::ErrorUnion { .. } = self.arena.get(t) {
            self.error(
                expr.span(),
                "error union must be handled with `try`, `catch`, or `_ =`",
            );
        }
    }

    /// `true` if `expr` is exactly a `@compileError(...)` / `@panic(...)` call —
    /// the unconditional, statement-position diverging builtins the comptime
    /// engine must execute so their message fires at the point reached.
    fn is_diverging_builtin_stmt(expr: &Expr) -> bool {
        matches!(expr, Expr::Builtin { name, .. } if name == "@compileError" || name == "@panic")
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
                    let found_s = self.arena.fmt(vt);
                    self.error_rich(
                        Diagnostic::error(
                            value.span(),
                            format!(
                                "expected `{}`, found `{found_s}`",
                                self.arena.fmt(target_ty)
                            ),
                        )
                        .with_primary_label(format!("this is `{found_s}`")),
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
            ContainerKind::Struct {
                is_extern,
                is_packed,
            } => self.eval_struct(c, name, *is_extern, *is_packed),
            ContainerKind::Enum { tag } => self.eval_enum(c, name, tag.as_deref()),
            ContainerKind::Union { tag } => self.eval_union(c, name, tag),
        }
    }

    /// Evaluates a `struct {...}` body into a [`Type::Struct`].
    fn eval_struct(
        &mut self,
        c: &Container,
        name: &str,
        is_extern: bool,
        is_packed: bool,
    ) -> TypeId {
        let mut fields = Vec::new();
        for m in &c.members {
            if let Member::Field(f) = m {
                let ty = match &f.ty {
                    Some(t) => self.eval_type(t),
                    None => self.arena.t_deferred(),
                };
                let align = self.eval_field_align_static(f);
                fields.push(FieldInfo {
                    name: f.name.clone(),
                    ty,
                    has_default: f.default.is_some(),
                    is_comptime: f.is_comptime,
                    align,
                    bit_offset: None,
                    bit_width: None,
                    span: f.span,
                });
            }
        }
        let layout = struct_layout_of(is_extern, is_packed);
        if layout == StructLayout::Packed {
            self.fill_packed_offsets(c.span, &mut fields);
        }
        let info = StructInfo {
            def: self.def_of(c.span),
            name: name.to_string(),
            span: c.span,
            layout,
            fields,
            decls: Vec::new(),
        };
        let ty = self.arena.intern_struct(info);
        self.eval_container_decls(c, ty);
        ty
    }

    /// Evaluates a struct field's `align(N)` clause (static, non-generic path) to
    /// its byte count, diagnosing a non-power-of-two value. `None` => no clause.
    fn eval_field_align_static(&mut self, f: &k2_syntax::Field) -> Option<u64> {
        let expr = f.align.as_ref()?;
        self.eval_align_expr(expr)
    }

    /// Evaluates a struct field's `align(N)` clause in a comptime env (the
    /// generic-instantiation path), reusing the same power-of-two validation.
    pub(crate) fn eval_field_align(
        &mut self,
        _env: &mut crate::comptime::Env,
        f: &k2_syntax::Field,
    ) -> Option<u64> {
        // The align expression of a generic field is almost always a literal or a
        // `const` — reuse the static comptime evaluator, which already runs in a
        // fresh env.
        let expr = f.align.as_ref()?;
        self.eval_align_expr(expr)
    }

    /// Folds an `align(N)` expression to a byte count, requiring a positive
    /// power-of-two (spec §03). A malformed value is diagnosed and dropped
    /// (treated as "no explicit align"), never silently honored.
    fn eval_align_expr(&mut self, expr: &Expr) -> Option<u64> {
        let v = self.comptime_eval_value(expr).and_then(|v| v.as_int());
        match v {
            Some(n) if n > 0 && (n as u128).is_power_of_two() => Some(n as u64),
            Some(n) => {
                self.error(
                    expr.span(),
                    format!("`align({n})` must be a positive power of two"),
                );
                None
            }
            None => {
                // A non-comptime align is a diagnostic, not a silent default.
                self.error(expr.span(), "`align(...)` must be comptime-known");
                None
            }
        }
    }

    /// Fills `bit_offset`/`bit_width` for every field of a `packed struct`,
    /// LSB-first, in declaration order (spec §02). A field whose type is not
    /// bit-addressable (a pointer/slice/array/aggregate other than a nested
    /// packed struct) is a diagnostic; the total backing width is capped at 128
    /// bits (the widest integer both backends represent).
    pub(crate) fn fill_packed_offsets(&mut self, span: Span, fields: &mut [FieldInfo]) {
        let mut bit = 0u64;
        for f in fields.iter_mut() {
            let signed = matches!(self.arena.get(f.ty), Type::Int { signed: true, .. });
            match crate::reflect::packed_bit_width(&self.arena, f.ty) {
                Some(w) => {
                    f.bit_offset = Some(bit as u32);
                    f.bit_width = Some(w as u32);
                    bit += w;
                }
                None => {
                    let fname = f.name.clone();
                    self.error(
                        f.span,
                        format!(
                            "packed-struct field `{fname}` must be an integer, bool, enum, \
                             or nested packed-struct type"
                        ),
                    );
                    f.bit_offset = Some(bit as u32);
                    f.bit_width = Some(0);
                }
            }
            let _ = signed;
        }
        if bit > 128 {
            self.error(
                span,
                format!("packed struct is {bit} bits wide; the maximum is 128 bits"),
            );
        }
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
            // Inferred backing: the smallest unsigned int that holds the variant
            // count, so `@sizeOf(enum{a,b,c})` is the minimal 1 byte (and
            // `@typeInfo(E).tag_type` matches) rather than an 8-byte `usize`.
            None => self.inferred_enum_tag(variants.len()),
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

    /// The interned tag type for an inferred-tag enum with `n` variants: the
    /// smallest unsigned integer that distinguishes the variants, `bits =
    /// max(1, ceil(log2(n)))`. So a 1..=2-variant enum gets `u1`, 3..=4 -> `u2`,
    /// …, giving a minimal-fit layout (`@sizeOf == 1` for a 3-variant enum)
    /// instead of an 8-byte `usize`.
    pub(crate) fn inferred_enum_tag(&mut self, n: usize) -> TypeId {
        let bits = enum_tag_bits(n);
        self.arena.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(bits),
        })
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

    /// Records the collected member declarations into a nominal Info (public to
    /// the comptime engine, which builds instantiated containers).
    pub(crate) fn store_decls_pub(&mut self, container_ty: TypeId, decls: Vec<MemberDecl>) {
        self.store_decls(container_ty, decls);
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

/// The minimal unsigned tag width for an inferred-tag enum of `n` variants:
/// `max(1, ceil(log2(n)))` bits (so 0..=2 variants -> 1 bit, 3..=4 -> 2, …). A
/// zero-variant enum still gets a 1-bit tag (a legal, minimal placeholder).
fn enum_tag_bits(n: usize) -> u16 {
    if n <= 2 {
        return 1;
    }
    // ceil(log2(n)): bits needed to index `n` distinct variants.
    let bits = (usize::BITS - (n - 1).leading_zeros()) as u16;
    bits.max(1)
}
