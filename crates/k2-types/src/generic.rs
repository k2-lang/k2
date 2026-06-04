//! Generic instantiation (monomorphization) with an identity-stable cache.
//!
//! A generic in k2 is `fn(comptime T: type) <ret>` — including
//! `fn(...) type` returning a struct/enum/union type (spec §07.4). When such a
//! function is called with comptime-known arguments, this module *instantiates*
//! it: binds the comptime params to their argument [`Value`]s, evaluates the
//! body, and yields a concrete type/value. Instantiations are cached by
//! `(fn DefId, tuple of comptime arg Values)`, so the same arguments reuse one
//! result — making `List(u32) == List(u32)` a single interned [`TypeId`]
//! (identity-stable types, spec §07.4.3) and reporting a failed instantiation
//! exactly once.
//!
//! The trigger is unchanged from v0.5: a call whose callee signature has a
//! `comptime`/`anytype` parameter. Where v0.5 returned
//! [`Deferred`](crate::ty::Type::Deferred), the checker now calls
//! [`Checker::instantiate_call`]; if the comptime args are not comptime-known it
//! still falls back to `Deferred` (preserving std/`sys`/`build` opacity).

use std::collections::HashMap;

use k2_resolve::DefId;
use k2_syntax::{Container, ContainerKind, Expr, Item, Stmt};

use crate::comptime::{Diverge, Env};
use crate::ty::{FnSig, Type, TypeId};
use crate::value::Value;

/// A normalized comptime argument, used as part of an instantiation cache key.
/// `Type(TypeId)` works because the arena interns `u32` for `u32` once, so two
/// `List(u32)` calls produce the same key argument.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum InstArg {
    /// A `type` argument (the common case for generics).
    Type(TypeId),
    /// A comptime integer argument (e.g. `Buffer(64)`).
    Int(i128),
    /// A comptime bool argument.
    Bool(bool),
    /// A comptime string argument.
    Str(String),
    /// Any argument that is concrete but not separately keyed (its declared
    /// type stands in); two such calls with the same declared types share a key.
    Other(TypeId),
}

/// The cache key: the callee fn plus the tuple of comptime/typed arguments.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct InstKey {
    /// The generic function's definition.
    pub fn_def: DefId,
    /// The normalized argument tuple.
    pub args: Vec<InstArg>,
}

/// The result of an instantiation, cached so repeated calls reuse it.
#[derive(Clone, Debug)]
pub(crate) enum InstResult {
    /// The instantiation produced a type (a type-returning generic).
    Type(TypeId),
    /// The instantiation produced a non-type value.
    Value(Value),
    /// The instantiation reported a type error / `@compileError` (cached so it
    /// is reported once, not per use site).
    Failed,
}

impl crate::check::Checker<'_> {
    // =====================================================================
    //  Call evaluation (comptime path)
    // =====================================================================

    /// Evaluates a call expression at comptime: a generic instantiation, or a
    /// call to a comptime-evaluable ordinary function.
    pub(crate) fn eval_call(
        &mut self,
        env: &mut Env,
        callee: &Expr,
        args: &[Expr],
        span: k2_syntax::Span,
    ) -> Result<Value, Diverge> {
        // Resolve the callee to a fn definition.
        let Some(fn_def) = self.callee_fn_def(callee) else {
            return Err(Diverge::NotComptime);
        };
        // Evaluate the arguments to comptime values (each must be comptime-known
        // for a comptime call).
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            arg_vals.push(self.eval_expr(env, a)?);
        }
        self.instantiate_fn(fn_def, &arg_vals, span)
    }

    /// Resolves a call's callee expression to the fn definition it names, if it
    /// is a directly-named function with an indexed AST.
    fn callee_fn_def(&self, callee: &Expr) -> Option<DefId> {
        match callee {
            // A bare generic-fn call: `List(u32)`.
            Expr::Ident { span, .. } => {
                if let Some(k2_resolve::Resolution::Def(id)) = self.resolution_at(*span) {
                    if self.fn_items.contains_key(&id) {
                        return Some(id);
                    }
                }
                None
            }
            // A namespaced generic-fn call: `std.ArrayList(u32)`. The field access
            // was resolved (by the namespace-member path) to the member's decl,
            // recorded as a `MemberRes::Decl`. This is what lets a bundled-std
            // generic container instantiate through the normal engine, exactly
            // like a user's own `List(T)`.
            Expr::Field { span, .. } => {
                if let Some(crate::ty::MemberRes::Decl(id)) =
                    self.members.get(&(span.start, span.end)).copied()
                {
                    if self.fn_items.contains_key(&id) {
                        return Some(id);
                    }
                }
                None
            }
            _ => None,
        }
    }

    // =====================================================================
    //  Instantiation
    // =====================================================================

    /// Instantiates `fn_def` with the given comptime argument values, returning
    /// the body's result value (a `type` for a type-returning generic).
    pub(crate) fn instantiate_fn(
        &mut self,
        fn_def: DefId,
        arg_vals: &[Value],
        span: k2_syntax::Span,
    ) -> Result<Value, Diverge> {
        let key = self.inst_key(fn_def, arg_vals);
        if let Some(cached) = self.inst_cache.get(&key) {
            return match cached.clone() {
                InstResult::Type(t) => Ok(Value::Type(t)),
                InstResult::Value(v) => Ok(v),
                InstResult::Failed => Err(Diverge::CompileError),
            };
        }
        // Recursion / depth guard (the fuel budget is the primary bound).
        if self.inst_stack.len() >= 64 {
            return Err(Diverge::NotComptime);
        }

        let item = self.fn_items.get(&fn_def).cloned();
        let Some(item) = item else {
            return Err(Diverge::NotComptime);
        };
        let Item::Fn { params, body, .. } = &item else {
            return Err(Diverge::NotComptime);
        };
        let Some(body) = body else {
            return Err(Diverge::NotComptime);
        };

        // Bind the comptime/value params into a fresh env frame.
        let mut env = Env::new();
        for (p, v) in params.iter().zip(arg_vals.iter()) {
            if let Some(def) = self.def_of(p.span) {
                env.define(def, v.clone());
            }
        }

        self.inst_stack.push((fn_def, span));
        let result = self.run_instantiation_body(body, &mut env);
        self.inst_stack.pop();

        match result {
            Ok(v) => {
                let cached = match &v {
                    Value::Type(t) => InstResult::Type(*t),
                    other => InstResult::Value(other.clone()),
                };
                self.inst_cache.insert(key, cached);
                Ok(v)
            }
            Err(Diverge::CompileError) | Err(Diverge::Fuel) => {
                // A diagnosed failure: cache so it is reported once.
                self.inst_cache.insert(key, InstResult::Failed);
                Err(Diverge::CompileError)
            }
            Err(other) => Err(other),
        }
    }

    /// Runs a generic body to its `return` value, threading the bound env.
    fn run_instantiation_body(&mut self, body: &[Stmt], env: &mut Env) -> Result<Value, Diverge> {
        for s in body {
            match self.eval_stmt(env, s) {
                Ok(()) => {}
                Err(Diverge::Return(v)) => return Ok(v),
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Void)
    }

    /// Builds the instantiation cache key from the argument values.
    fn inst_key(&self, fn_def: DefId, arg_vals: &[Value]) -> InstKey {
        let args = arg_vals
            .iter()
            .map(|v| match v {
                Value::Type(t) => InstArg::Type(*t),
                Value::Int(ci) => InstArg::Int(ci.v),
                Value::Bool(b) => InstArg::Bool(*b),
                Value::Str(s) => InstArg::Str(s.clone()),
                Value::Enum { ty, which } => {
                    InstArg::Int(((ty.0 as i128) << 32) | (*which as i128))
                }
                _ => InstArg::Other(self.arena.t_deferred()),
            })
            .collect();
        InstKey { fn_def, args }
    }

    // =====================================================================
    //  Container evaluation with comptime bindings
    // =====================================================================

    /// Evaluates a container (`struct`/`enum`/`union`) body with the comptime
    /// environment in scope, producing a *fresh nominal type per distinct
    /// instantiation*. The disambiguation is the per-instance synthetic span:
    /// the generic body has one source span, but each argument tuple yields
    /// distinct field types, so a fresh span makes the arena intern a distinct
    /// struct, while the instantiation cache guarantees the *same* tuple reuses
    /// one type (identity-stability).
    pub(crate) fn eval_container_comptime(&mut self, env: &mut Env, c: &Container) -> TypeId {
        let display = self.current_inst_display();
        match &c.kind {
            ContainerKind::Struct { is_extern } => {
                self.eval_struct_comptime(env, c, &display, *is_extern)
            }
            ContainerKind::Enum { tag } => {
                self.eval_enum_comptime(env, c, &display, tag.as_deref())
            }
            ContainerKind::Union { .. } => {
                // Unions inside generics are rare in the corpus; fall back to the
                // static evaluator (its fields will be Deferred where they depend
                // on the bound params, which stays sound).
                self.eval_container(c, &display)
            }
        }
    }

    /// Evaluates a `struct {...}` body with comptime params bound, interning a
    /// fresh nominal struct keyed by a synthetic per-instance span.
    fn eval_struct_comptime(
        &mut self,
        env: &mut Env,
        c: &Container,
        display: &str,
        is_extern: bool,
    ) -> TypeId {
        let mut fields = Vec::new();
        for m in &c.members {
            if let k2_syntax::Member::Field(f) = m {
                let ty = match &f.ty {
                    Some(t) => self
                        .eval_type_comptime(env, t)
                        .unwrap_or_else(|_| self.arena.t_deferred()),
                    None => self.arena.t_deferred(),
                };
                fields.push(crate::ty::FieldInfo {
                    name: f.name.clone(),
                    ty,
                    has_default: f.default.is_some(),
                    is_comptime: f.is_comptime,
                    span: f.span,
                });
            }
        }
        let span = self.fresh_synthetic_span();
        let info = crate::ty::StructInfo {
            def: self.def_of(c.span),
            name: display.to_string(),
            span,
            is_extern,
            fields,
            decls: Vec::new(),
        };
        let struct_ty = self.arena.intern_struct(info);
        // Build the nested method/const declarations with the comptime params
        // and the freshly-instantiated `Self` bound, so a method signature like
        // `fn push(self: *Self, value: T) !void` gets `T` substituted and member
        // access (`nums.push(40)`) checks against the concrete element type.
        let decls = self.eval_container_decls_comptime(env, c, struct_ty);
        self.store_decls_pub(struct_ty, decls);
        // Per-instantiation soundness (spec §07.4): re-type-check each method body
        // with the concrete `Self`/`T`/field types bound, so a body that is
        // ill-typed only for *this* instantiation (e.g. `self.val * self.val`
        // when `T = bool`) is reported. Cached by `struct_ty` so a shared
        // instantiation is checked once; the `inst_stack` depth bound (and the
        // fuel budget) guard against unbounded recursive instantiation.
        self.recheck_instantiated_methods(env, c, struct_ty);
        struct_ty
    }

    /// Re-type-checks the method bodies of a freshly-built instantiated struct
    /// with the concrete substitutions in scope, exactly once per distinct
    /// instantiation. The instantiated struct's `Self`, field types, and method
    /// param types are all concrete here, so the ordinary body checker
    /// (`check_stmt`) surfaces a per-`T` type error with the instantiation
    /// context already attached by `comptime_error`/the diagnostic path.
    fn recheck_instantiated_methods(&mut self, env: &mut Env, c: &Container, struct_ty: TypeId) {
        if !self.rechecked_insts.insert(struct_ty) {
            return; // Already checked this instantiation.
        }
        // The method DefIds are shared by *every* instantiation and by the
        // generic's own static (T = type) body check, so any `binding_types` /
        // `item_types` entry we overwrite for the concrete `T`/params/Self MUST
        // be restored afterward — otherwise the later static check would see the
        // concrete `T` and spuriously report (or miss) errors. We snapshot the
        // prior values and restore them on exit.
        let mut saved_bindings: Vec<(DefId, Option<TypeId>)> = Vec::new();
        let mut saved_items: Vec<(DefId, Option<TypeId>)> = Vec::new();
        let set_binding =
            |this: &mut Self, saved: &mut Vec<(DefId, Option<TypeId>)>, def: DefId, t: TypeId| {
                saved.push((def, this.binding_types.get(&def).copied()));
                this.binding_types.insert(def, t);
            };

        // Bind the outer generic comptime params (`T`) into scope: value-type
        // `type`, denoted type the concrete argument.
        for (def, val) in env.iter_bindings() {
            if let Value::Type(t) = val {
                let type_t = self.arena.t_type();
                set_binding(self, &mut saved_bindings, def, type_t);
                saved_items.push((def, self.item_types.get(&def).copied()));
                self.item_types.insert(def, t);
            }
        }

        self.self_stack.push(struct_ty);
        for m in &c.members {
            if let k2_syntax::Member::Decl(Item::Fn {
                params, ret, body, ..
            }) = m
            {
                let Some(body) = body else { continue };
                // Rebuild the signature with the comptime env in scope so params
                // like `self: *Self` / `value: T` get concrete types, then bind
                // each param and walk the body under a matching fn frame.
                let sig = self.build_fn_sig_comptime(env, params, ret);
                for (p, info) in params.iter().zip(sig.params.iter()) {
                    if let Some(def) = self.def_of(p.span) {
                        set_binding(self, &mut saved_bindings, def, info.ty);
                    }
                }
                let ret_ty = sig.ret;
                let (is_eu, ok, err) = self.error_union_parts(ret_ty);
                self.push_fn_frame(ret_ty, is_eu, ok, err);
                for s in body {
                    self.check_stmt(s);
                }
                self.pop_fn_frame();
            }
        }
        self.self_stack.pop();

        // Restore every overwritten binding so the shared DefIds are unchanged
        // for the static generic-body check and other instantiations.
        for (def, prev) in saved_bindings.into_iter().rev() {
            match prev {
                Some(t) => {
                    self.binding_types.insert(def, t);
                }
                None => {
                    self.binding_types.remove(&def);
                }
            }
        }
        for (def, prev) in saved_items.into_iter().rev() {
            match prev {
                Some(t) => {
                    self.item_types.insert(def, t);
                }
                None => {
                    self.item_types.remove(&def);
                }
            }
        }
    }

    /// Builds the nested const/var/fn member declarations of a comptime-evaluated
    /// container with the comptime env + instantiated `Self` in scope. Method
    /// bodies are NOT re-checked here (the instantiation result is the type); the
    /// member *signatures* are what member access needs.
    fn eval_container_decls_comptime(
        &mut self,
        env: &mut Env,
        c: &Container,
        container_ty: TypeId,
    ) -> Vec<crate::ty::MemberDecl> {
        self.self_stack.push(container_ty);
        let mut decls = Vec::new();
        for m in &c.members {
            if let k2_syntax::Member::Decl(item) = m {
                match item {
                    Item::Fn {
                        name,
                        params,
                        ret,
                        is_pub,
                        span,
                        ..
                    } => {
                        let sig = self.build_fn_sig_comptime(env, params, ret);
                        let fnty = self.arena.intern_fn(sig);
                        if let Some(def) = self.def_of(*span) {
                            self.binding_types.insert(def, fnty);
                            decls.push(crate::ty::MemberDecl {
                                name: name.clone(),
                                is_pub: *is_pub,
                                def,
                                ty: fnty,
                            });
                        }
                    }
                    Item::Const {
                        name,
                        value,
                        is_pub,
                        span,
                        ..
                    } => {
                        // `const Self = @This()` and similar denote a type.
                        let t = match self.eval_type_comptime(env, value) {
                            Ok(t) if !self.arena.is_bottom(t) => t,
                            _ => self.arena.t_type(),
                        };
                        if let Some(def) = self.def_of(*span) {
                            self.binding_types.insert(def, self.arena.t_type());
                            self.item_types.insert(def, t);
                            decls.push(crate::ty::MemberDecl {
                                name: name.clone(),
                                is_pub: *is_pub,
                                def,
                                ty: self.arena.t_type(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        self.self_stack.pop();
        decls
    }

    /// Builds a [`FnSig`] from AST params/ret with the comptime env in scope, so
    /// type params (`T`) and `Self` are substituted concretely.
    fn build_fn_sig_comptime(
        &mut self,
        env: &mut Env,
        params: &[k2_syntax::Param],
        ret: &Expr,
    ) -> FnSig {
        let mut infos = Vec::with_capacity(params.len());
        let mut has_comptime = false;
        let mut has_anytype = false;
        for p in params {
            let ty = self
                .eval_type_comptime(env, &p.ty)
                .unwrap_or_else(|_| self.arena.t_deferred());
            if p.is_comptime {
                has_comptime = true;
            }
            if matches!(self.arena.get(ty), Type::AnyType) {
                has_anytype = true;
            }
            infos.push(crate::ty::ParamInfo {
                name: p.name.clone(),
                ty,
                is_comptime: p.is_comptime,
                span: p.span,
            });
        }
        let ret_ty = self
            .eval_type_comptime(env, ret)
            .unwrap_or_else(|_| self.arena.t_deferred());
        FnSig {
            params: infos,
            is_varargs: false,
            ret: ret_ty,
            has_comptime_param: has_comptime,
            has_anytype_param: has_anytype,
        }
    }

    /// Evaluates an `enum {...}` body with comptime params bound.
    fn eval_enum_comptime(
        &mut self,
        env: &mut Env,
        c: &Container,
        display: &str,
        tag: Option<&Expr>,
    ) -> TypeId {
        let mut variants = Vec::new();
        for m in &c.members {
            if let k2_syntax::Member::Field(f) = m {
                variants.push(crate::ty::EnumVariant {
                    name: f.name.clone(),
                    span: f.span,
                });
            }
        }
        let tag_ty = match tag {
            Some(t) => self
                .eval_type_comptime(env, t)
                .unwrap_or_else(|_| self.arena.t_usize()),
            // Inferred backing: minimal unsigned tag (see `inferred_enum_tag`).
            None => self.inferred_enum_tag(variants.len()),
        };
        let span = self.fresh_synthetic_span();
        let info = crate::ty::EnumInfo {
            def: self.def_of(c.span),
            name: display.to_string(),
            span,
            tag: tag_ty,
            variants,
            decls: Vec::new(),
        };
        self.arena.intern_enum(info)
    }

    /// A display name for the type produced by the current instantiation, e.g.
    /// `List(u32)`, derived from the in-progress instantiation's fn name and the
    /// bound type arguments. Falls back to `struct` outside an instantiation.
    fn current_inst_display(&self) -> String {
        if let Some((fn_def, _)) = self.inst_stack.last() {
            let name = self.resolved.defs[fn_def.index()].name.clone();
            return name;
        }
        "struct".to_string()
    }

    // =====================================================================
    //  The checker-driven entry: replace the generic-call Deferral
    // =====================================================================

    /// Attempts to instantiate a generic call into a concrete result type. On
    /// success returns the instantiated type; on a comptime-unknown argument
    /// returns [`Type::Deferred`]; on a diagnosed failure returns
    /// [`Type::Error`]. Always synthesizes the arguments first (to catch concrete
    /// per-argument errors, preserving v0.5 behavior).
    pub(crate) fn instantiate_call(
        &mut self,
        callee: &Expr,
        sig: &FnSig,
        args: &[Expr],
        span: k2_syntax::Span,
    ) -> TypeId {
        // Synthesize each argument for concrete-error detection.
        for a in args {
            self.synth(a);
        }
        let Some(fn_def) = self.callee_fn_def(callee) else {
            return self.arena.t_deferred();
        };
        // Evaluate comptime/anytype arguments. A comptime param needs a comptime
        // value; an ordinary param under a generic still needs a value to bind,
        // but we only require the comptime ones to be known.
        self.reset_fuel();
        let mut arg_vals = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let is_comptime = sig.params.get(i).map(|p| p.is_comptime).unwrap_or(false);
            let is_anytype = sig
                .params
                .get(i)
                .map(|p| matches!(self.arena.get(p.ty), Type::AnyType))
                .unwrap_or(false);
            let mut env = Env::new();
            match self.eval_expr(&mut env, a) {
                Ok(v) => arg_vals.push(v),
                Err(Diverge::NotComptime) => {
                    if is_comptime {
                        // A comptime param with a non-comptime arg: cannot
                        // instantiate; stay Deferred.
                        return self.arena.t_deferred();
                    }
                    // A runtime arg to an anytype/ordinary param: bind a value
                    // typed by its synth'd type so the body can still check.
                    let t = self.synth(a);
                    if is_anytype {
                        arg_vals.push(Value::Type(t));
                    } else {
                        arg_vals.push(Value::Undefined(t));
                    }
                }
                Err(_) => return self.arena.t_error(),
            }
        }

        match self.instantiate_fn(fn_def, &arg_vals, span) {
            Ok(Value::Type(t)) => {
                // A type-returning generic (`List(u32)`): the call expression's
                // *value* is a `type`, so record the span as type-denoting. The
                // returned struct/enum/union id is what member access and a
                // type-position use resolve against.
                self.type_valued_spans.insert((span.start, span.end), t);
                t
            }
            Ok(_) => {
                // A generic *function* (e.g. `maxOf(i32,3,9)`): the result type is
                // the declared return type with comptime params substituted.
                self.instantiated_return_type(sig, &arg_vals)
            }
            Err(Diverge::NotComptime) => self.arena.t_deferred(),
            Err(_) => self.arena.t_error(),
        }
    }

    /// The concrete return type of an instantiated generic *function*: if the
    /// declared return type is a bare comptime type-param, substitute its bound
    /// `type` value; otherwise keep the declared return type.
    fn instantiated_return_type(&mut self, sig: &FnSig, arg_vals: &[Value]) -> TypeId {
        // Find a comptime `type` param whose name matches the return type's
        // denotation. A precise substitution would walk the return expr; for the
        // corpus's `maxOf(comptime T, a: T, b: T) T`, the return type is `T`.
        for (i, p) in sig.params.iter().enumerate() {
            if p.is_comptime {
                if let Some(Value::Type(t)) = arg_vals.get(i) {
                    if sig.ret == p.ty || matches!(self.arena.get(sig.ret), Type::Deferred) {
                        return *t;
                    }
                }
            }
        }
        if self.arena.is_bottom(sig.ret) {
            self.arena.t_deferred()
        } else {
            sig.ret
        }
    }
}

/// The instantiation cache type alias.
pub(crate) type InstCache = HashMap<InstKey, InstResult>;
