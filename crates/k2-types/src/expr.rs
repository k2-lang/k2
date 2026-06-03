//! The bidirectional expression rules: `synth` (bottom-up inference) and `check`
//! (against an expectation), plus the per-`Expr` typing rules for operators,
//! postfix forms, calls, indexing, control flow, and the captures they bind.

use k2_syntax::{BinOp, Expr, ForOperand, InitBody, Span, UnOp};

use crate::ty::{ErrSetRef, Type, TypeId};

impl crate::check::Checker<'_> {
    /// Checks `e` against an expected type, reporting a coercion error on
    /// failure. Returns the type actually used.
    pub(crate) fn check(&mut self, e: &Expr, expected: TypeId) -> TypeId {
        // Bidirectional special-casing for literals/forms that need the target.
        match e {
            Expr::Int { .. } => return self.check_int_literal(e, expected),
            Expr::Null { span } => return self.check_null(*span, expected),
            Expr::Undefined { span } => {
                // `undefined` coerces to any annotated type.
                self.record(*span, expected);
                return expected;
            }
            Expr::EnumLiteral { name, span } => {
                return self.check_enum_literal(name, *span, expected)
            }
            Expr::Init {
                ty: None,
                body,
                span,
            } => return self.check_anon_init(body, *span, expected),
            Expr::If { .. } | Expr::Switch { .. } | Expr::Block { .. } | Expr::Catch { .. } => {
                // Push the expectation into branches/arms/fallbacks.
                let got = self.synth_with_expectation(e, Some(expected));
                return self.finish_check(e.span(), got, expected);
            }
            _ => {}
        }
        let got = self.synth(e);
        // Comptime-int overflow at a sized-int coercion site: a fully
        // compile-time-known value that does not fit the target is a compile error
        // (spec §02), not a wraparound. Literals are already range-checked above;
        // here we catch non-literal comptime_int values produced by const bindings
        // and constant folding (`const big = 200 + 200; ...: u8`). Unknown /
        // non-foldable values keep the existing conservative behavior.
        if matches!(self.arena.get(got), Type::ComptimeInt) {
            if let Type::Int { signed, bits } = self.arena.get(expected).clone() {
                if let Some(v) = self.fold_comptime_int(e) {
                    if !crate::coerce::int_fits_pub(v, signed, bits) {
                        let (lo, hi) = crate::coerce::int_range_pub(signed, bits);
                        self.error(
                            e.span(),
                            format!(
                                "comptime integer value `{}` out of range for `{}` ({}..={})",
                                v,
                                self.arena.fmt(expected),
                                lo,
                                hi
                            ),
                        );
                        return self.arena.t_error();
                    }
                }
            }
        }
        self.finish_check(e.span(), got, expected)
    }

    /// Emits a coercion diagnostic if `got` does not coerce to `expected`.
    fn finish_check(&mut self, span: Span, got: TypeId, expected: TypeId) -> TypeId {
        if self.arena.coerces(got, expected) {
            // Prefer recording the more-specific concrete type.
            let rec = if self.arena.is_bottom(got) {
                expected
            } else {
                got
            };
            self.record(span, rec);
            got
        } else {
            self.error(
                span,
                format!(
                    "expected `{}`, found `{}`",
                    self.arena.fmt(expected),
                    self.arena.fmt(got)
                ),
            );
            self.arena.t_error()
        }
    }

    /// Infers an expression's type bottom-up.
    pub(crate) fn synth(&mut self, e: &Expr) -> TypeId {
        self.synth_with_expectation(e, None)
    }

    /// Inference core, optionally threading an expectation into control-flow
    /// branch joins. Most expressions ignore the expectation.
    pub(crate) fn synth_with_expectation(&mut self, e: &Expr, expected: Option<TypeId>) -> TypeId {
        if !self.enter_expr() {
            return self.arena.t_deferred();
        }
        let ty = self.synth_inner(e, expected);
        self.record(e.span(), ty);
        self.leave_expr();
        ty
    }

    /// The match over expression variants.
    fn synth_inner(&mut self, e: &Expr, expected: Option<TypeId>) -> TypeId {
        match e {
            // ---- Literals ----------------------------------------------
            Expr::Int { .. } => self.arena.t_comptime_int(),
            Expr::Float { .. } => self.arena.t_comptime_float(),
            Expr::Str { .. } => self.arena.t_str(),
            Expr::Char { .. } => self.arena.t_comptime_int(),
            Expr::Bool { .. } => self.arena.t_bool(),
            Expr::Null { .. } => {
                let d = self.arena.t_deferred();
                self.arena.optional(d)
            }
            Expr::Undefined { .. } => self.arena.t_deferred(),
            Expr::Unreachable { .. } => self.arena.t_noreturn(),

            // ---- Names -------------------------------------------------
            Expr::Ident { name, span } => self.synth_ident(name, *span),

            // ---- Member / postfix --------------------------------------
            Expr::Field { base, field, span } => self.synth_field(base, field, *span),
            Expr::EnumLiteral { span, .. } => self.synth_enum_literal(*span),
            Expr::ErrorLiteral { name, span } => self.synth_error_literal(name, *span),
            Expr::Index { base, index, span } => self.synth_index(base, index, *span),
            Expr::SliceExpr { base, lo, hi, span } => {
                self.synth_slice_expr(base, lo, hi.as_deref(), *span)
            }
            Expr::Deref { base, span } => self.synth_deref(base, *span),
            Expr::Unwrap { base, span } => self.synth_unwrap(base, *span),

            // ---- Calls / builtins --------------------------------------
            Expr::Call { callee, args, span } => self.synth_call(callee, args, *span),
            Expr::Builtin { name, args, span } => self.synth_builtin(name, args, *span),

            // ---- Operators ---------------------------------------------
            Expr::Binary { op, lhs, rhs, span } => self.synth_binary(*op, lhs, rhs, *span),
            Expr::Unary { op, operand, span } => self.synth_unary(*op, operand, *span),

            // ---- Error handling expressions ----------------------------
            Expr::Catch {
                lhs,
                capture,
                rhs,
                span,
            } => self.synth_catch(lhs, capture.as_deref(), rhs, *span, expected),

            // ---- Initializers ------------------------------------------
            Expr::Init { ty, body, span } => self.synth_init(ty.as_deref(), body, *span),

            // ---- comptime ----------------------------------------------
            Expr::Comptime { inner, .. } => self.synth(inner),

            // ---- Type-constructor expressions (their value is a type) --
            Expr::Optional { .. }
            | Expr::Pointer { .. }
            | Expr::Slice { .. }
            | Expr::ArrayType { .. }
            | Expr::ErrorUnion { .. }
            | Expr::FnType { .. }
            | Expr::ErrorSet { .. }
            | Expr::AnyType { .. }
            | Expr::Container(_) => {
                // Evaluate the denoted type for its side effects (interning,
                // sub-checks), but the *value* of a type expression is `type`.
                let _ = self.eval_type(e);
                self.arena.t_type()
            }

            // ---- Control flow in expression position -------------------
            Expr::Block { label, body, span } => {
                self.synth_block(label.as_deref(), body, *span, expected)
            }
            Expr::If {
                cond,
                capture,
                then_branch,
                else_capture,
                else_branch,
                span,
            } => self.synth_if(
                cond,
                capture.as_ref(),
                then_branch,
                else_capture.as_ref(),
                else_branch.as_deref(),
                *span,
                expected,
            ),
            Expr::While {
                cond,
                capture,
                cont,
                body,
                else_capture,
                else_branch,
                ..
            } => self.synth_while(
                cond,
                capture.as_ref(),
                cont.as_deref(),
                body,
                else_capture.as_ref(),
                else_branch.as_deref(),
            ),
            Expr::For {
                operands,
                captures,
                body,
                else_branch,
                ..
            } => self.synth_for(operands, captures, body, else_branch.as_deref()),
            Expr::Switch {
                scrutinee,
                arms,
                span,
            } => self.synth_switch(scrutinee, arms, *span, expected),
        }
    }

    // =====================================================================
    //  Names
    // =====================================================================

    /// Synthesizes an identifier reference via the resolver's `Resolution`.
    fn synth_ident(&mut self, name: &str, span: Span) -> TypeId {
        match self.resolution_at(span) {
            Some(k2_resolve::Resolution::Predeclared(_)) => {
                // A predeclared type name in value position has type `type`; a
                // predeclared value would too, but none exist in v0.5.
                self.predeclared_value_type(name)
            }
            Some(k2_resolve::Resolution::Def(id)) => self
                .binding_types
                .get(&id)
                .copied()
                .unwrap_or_else(|| self.arena.t_deferred()),
            Some(k2_resolve::Resolution::Module(id)) => {
                let mid = self.resolved.defs[id.index()]
                    .module
                    .unwrap_or(k2_resolve::ModuleId(0));
                self.arena.intern(Type::Module(mid))
            }
            Some(k2_resolve::Resolution::DeferredMember) => self.arena.t_deferred(),
            Some(k2_resolve::Resolution::Error) | None => self.arena.t_error(),
        }
    }

    /// The type *of* a predeclared name used in value position. A type name
    /// (`u32`, `bool`) has type `type`; the capability markers have their opaque
    /// type so member access on `sys`/`alloc` works.
    fn predeclared_value_type(&mut self, name: &str) -> TypeId {
        match name {
            "System" | "Allocator" | "Build" => self.arena.intern_opaque(name),
            _ => self.arena.t_type(),
        }
    }

    // =====================================================================
    //  Postfix
    // =====================================================================

    /// `base[index]` — index an array/slice/array-pointer.
    fn synth_index(&mut self, base: &Expr, index: &Expr, span: Span) -> TypeId {
        let bt = self.synth(base);
        let usize_t = self.arena.t_usize();
        self.check(index, usize_t);
        if self.arena.is_bottom(bt) {
            return self.arena.t_deferred();
        }
        match self.indexable_elem(bt) {
            Some(elem) => elem,
            None => {
                self.error(
                    span,
                    format!("cannot index a value of type `{}`", self.arena.fmt(bt)),
                );
                self.arena.t_error()
            }
        }
    }

    /// `base[lo..hi]` — sub-slice.
    fn synth_slice_expr(
        &mut self,
        base: &Expr,
        lo: &Expr,
        hi: Option<&Expr>,
        span: Span,
    ) -> TypeId {
        let bt = self.synth(base);
        let usize_t = self.arena.t_usize();
        self.check(lo, usize_t);
        if let Some(hi) = hi {
            self.check(hi, usize_t);
        }
        if self.arena.is_bottom(bt) {
            return self.arena.t_deferred();
        }
        match self.slice_of_base(bt) {
            Some(s) => s,
            None => {
                self.error(
                    span,
                    format!("cannot slice a value of type `{}`", self.arena.fmt(bt)),
                );
                self.arena.t_error()
            }
        }
    }

    /// `base.*` — dereference a pointer.
    fn synth_deref(&mut self, base: &Expr, span: Span) -> TypeId {
        let bt = self.synth(base);
        if self.arena.is_bottom(bt) {
            return self.arena.t_deferred();
        }
        match self.arena.get(bt).clone() {
            Type::Pointer { pointee, .. } => pointee,
            _ => {
                self.error(
                    span,
                    format!(
                        "cannot dereference a value of type `{}` (not a pointer)",
                        self.arena.fmt(bt)
                    ),
                );
                self.arena.t_error()
            }
        }
    }

    /// `base.?` — unwrap an optional.
    fn synth_unwrap(&mut self, base: &Expr, span: Span) -> TypeId {
        let bt = self.synth(base);
        if self.arena.is_bottom(bt) {
            return self.arena.t_deferred();
        }
        match self.arena.get(bt).clone() {
            Type::Optional(inner) => inner,
            _ => {
                self.error(
                    span,
                    format!("`.?` requires an optional, found `{}`", self.arena.fmt(bt)),
                );
                self.arena.t_error()
            }
        }
    }

    // =====================================================================
    //  Calls
    // =====================================================================

    /// `callee(args...)` — function call with arity and per-argument checks.
    fn synth_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> TypeId {
        let ct = self.synth(callee);
        if self.arena.is_bottom(ct) {
            for a in args {
                self.synth(a);
            }
            return self.arena.t_deferred();
        }
        match self.arena.get(ct).clone() {
            Type::Fn(id) => {
                let sig = self.arena.fnsigs[id.0 as usize].clone();
                // A generic instantiation (comptime/anytype param) defers.
                if sig.has_comptime_param || sig.has_anytype_param {
                    for a in args {
                        self.synth(a);
                    }
                    return self.arena.t_deferred();
                }
                // Method-call sugar: `value.method(args)` passes the receiver as
                // the implicit first parameter, so the explicit args check
                // against `sig.params[1..]`.
                let skip_receiver = self.is_method_call(callee);
                self.check_call_args(callee, &sig, args, span, skip_receiver);
                sig.ret
            }
            // A bound method whose receiver is implicit is not modeled in v0.5;
            // calling a non-fn concrete value is an error.
            _ => {
                for a in args {
                    self.synth(a);
                }
                self.error(
                    span,
                    format!("cannot call a value of type `{}`", self.arena.fmt(ct)),
                );
                self.arena.t_error()
            }
        }
    }

    /// `true` if `callee` is a `value.method` access that resolved to a method
    /// declaration on a concrete aggregate — so the call uses method-call sugar
    /// (the receiver is the implicit first argument). Resolved via the member
    /// table the preceding `synth(callee)` populated.
    fn is_method_call(&self, callee: &Expr) -> bool {
        if let Expr::Field { span, .. } = callee {
            return matches!(
                self.members.get(&(span.start, span.end)),
                Some(crate::ty::MemberRes::Decl(_))
            );
        }
        false
    }

    /// Checks call arity and each argument against its parameter type. When
    /// `skip_receiver` is set (method-call sugar), the explicit args check
    /// against the parameters after the implicit receiver.
    fn check_call_args(
        &mut self,
        callee: &Expr,
        sig: &crate::ty::FnSig,
        args: &[Expr],
        span: Span,
        skip_receiver: bool,
    ) {
        let fname = callee_name(callee);
        let params: &[crate::ty::ParamInfo] = if skip_receiver && !sig.params.is_empty() {
            &sig.params[1..]
        } else {
            &sig.params
        };
        if !sig.is_varargs && args.len() != params.len() {
            self.error(
                span,
                format!(
                    "function {} expects {} argument(s), found {}",
                    fname,
                    params.len(),
                    args.len()
                ),
            );
            for a in args {
                self.synth(a);
            }
            return;
        }
        for (i, a) in args.iter().enumerate() {
            if let Some(p) = params.get(i) {
                let ty = p.ty;
                self.check(a, ty);
            } else {
                self.synth(a);
            }
        }
    }

    // =====================================================================
    //  Operators
    // =====================================================================

    /// Binary operator typing.
    fn synth_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> TypeId {
        // `and`/`or` require bool operands.
        if matches!(op, BinOp::And | BinOp::Or) {
            let bool_t = self.arena.t_bool();
            self.check(lhs, bool_t);
            self.check(rhs, bool_t);
            return bool_t;
        }
        // `orelse` requires an optional lhs.
        if matches!(op, BinOp::Orelse) {
            let lt = self.synth(lhs);
            if self.arena.is_bottom(lt) {
                self.synth(rhs);
                return self.arena.t_deferred();
            }
            return match self.arena.get(lt).clone() {
                Type::Optional(inner) => {
                    self.check(rhs, inner);
                    inner
                }
                _ => {
                    self.synth(rhs);
                    self.error(
                        span,
                        format!(
                            "`orelse` requires an optional, found `{}`",
                            self.arena.fmt(lt)
                        ),
                    );
                    self.arena.t_error()
                }
            };
        }
        // `||` error-set merge in value position.
        if matches!(op, BinOp::ErrSetMerge) {
            return self.eval_type(&Expr::Binary {
                op,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs.clone()),
                span,
            });
        }

        let lt = self.synth(lhs);
        let rt = self.synth(rhs);
        if self.arena.is_bottom(lt) || self.arena.is_bottom(rt) {
            return self.bottom_of(lt, rt);
        }

        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                self.arith_result(op, lt, rt, span)
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if self.integral(lt) && self.integral(rt) {
                    self.unify_numeric(lt, rt)
                } else {
                    self.error(
                        span,
                        format!(
                            "bitwise operator on non-integer types `{}` and `{}`",
                            self.arena.fmt(lt),
                            self.arena.fmt(rt)
                        ),
                    );
                    self.arena.t_error()
                }
            }
            // Equality (`==`/`!=`) admits the equatable set; ordering
            // (`<`/`<=`/`>`/`>=`) requires both operands be numeric. Type identity
            // alone never grants ordering to a non-orderable type — `bool < bool`,
            // `void < void`, `struct < struct`, `[]u8 < []u8`, enum ordering, etc.
            // are all rejected.
            BinOp::Eq | BinOp::Ne => {
                if self.equatable(lt, rt) {
                    self.arena.t_bool()
                } else {
                    self.error(
                        span,
                        format!(
                            "cannot compare `{}` with `{}`",
                            self.arena.fmt(lt),
                            self.arena.fmt(rt)
                        ),
                    );
                    self.arena.t_bool()
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if self.orderable(lt, rt) {
                    self.arena.t_bool()
                } else {
                    self.error(
                        span,
                        format!(
                            "operator `{}` requires orderable (numeric) operands, found `{}` and `{}`",
                            binop_word(op),
                            self.arena.fmt(lt),
                            self.arena.fmt(rt)
                        ),
                    );
                    self.arena.t_bool()
                }
            }
            BinOp::Concat => {
                // Array/slice concat: result is a slice of the common element.
                match (self.arena.get(lt).clone(), self.arena.get(rt).clone()) {
                    (
                        Type::Array { elem: e1, .. } | Type::Slice { elem: e1, .. },
                        Type::Array { elem: e2, .. } | Type::Slice { elem: e2, .. },
                    ) if e1 == e2 => self.arena.slice(true, e1),
                    _ => {
                        self.error(
                            span,
                            format!(
                                "`++` requires array/slice operands of the same element, found `{}` and `{}`",
                                self.arena.fmt(lt),
                                self.arena.fmt(rt)
                            ),
                        );
                        self.arena.t_error()
                    }
                }
            }
            // `and`/`or`/`orelse`/`||` handled above.
            BinOp::And | BinOp::Or | BinOp::Orelse | BinOp::ErrSetMerge => unreachable!(),
        }
    }

    /// Arithmetic result type: both operands must be the same numeric type after
    /// comptime-int/float unification.
    fn arith_result(&mut self, op: BinOp, lt: TypeId, rt: TypeId, span: Span) -> TypeId {
        if self.numeric(lt) && self.numeric(rt) {
            if let Some(common) = self.try_unify_numeric(lt, rt) {
                return common;
            }
        }
        self.error(
            span,
            format!(
                "arithmetic operator `{}` on incompatible types `{}` and `{}`",
                binop_word(op),
                self.arena.fmt(lt),
                self.arena.fmt(rt)
            ),
        );
        self.arena.t_error()
    }

    /// Unify two numeric types: equal -> that; comptime adapts to the sized peer.
    pub(crate) fn try_unify_numeric(&self, a: TypeId, b: TypeId) -> Option<TypeId> {
        if a == b {
            return Some(a);
        }
        let at = self.arena.get(a);
        let bt = self.arena.get(b);
        match (at, bt) {
            (Type::ComptimeInt, Type::Int { .. })
            | (Type::ComptimeInt, Type::Float { .. })
            | (Type::ComptimeInt, Type::ComptimeFloat) => Some(b),
            (Type::Int { .. }, Type::ComptimeInt)
            | (Type::Float { .. }, Type::ComptimeInt)
            | (Type::ComptimeFloat, Type::ComptimeInt) => Some(a),
            (Type::ComptimeFloat, Type::Float { .. }) => Some(b),
            (Type::Float { .. }, Type::ComptimeFloat) => Some(a),
            _ => None,
        }
    }

    /// Unify, defaulting to the left type when no clean peer exists (used by
    /// bitwise ops, which already verified both are integral).
    fn unify_numeric(&self, a: TypeId, b: TypeId) -> TypeId {
        self.try_unify_numeric(a, b).unwrap_or(a)
    }

    /// `true` if two types may be ordered with `<`/`<=`/`>`/`>=`. Only numeric
    /// types are orderable in v0.5 (`comptime_int` folds to its sized peer); no
    /// aggregate, pointer, enum, bool, or void supports ordering.
    fn orderable(&self, a: TypeId, b: TypeId) -> bool {
        self.numeric(a) && self.numeric(b) && self.try_unify_numeric(a, b).is_some()
    }

    /// `true` if two types may be compared for equality with `==`/`!=`. This is the
    /// *equatable* set: identical types, mutually-unifiable numerics, `bool`,
    /// nominal enums of the SAME identity, pointers with the SAME pointee (with
    /// compatible const-ness), and error sets/literals. Distinct nominal enums,
    /// distinct pointees, and aggregate/slice/void/array operands are NOT
    /// equatable — type identity alone is not enough (a distinct `EnumId` /
    /// pointee makes the comparison meaningless).
    fn equatable(&self, a: TypeId, b: TypeId) -> bool {
        if a == b {
            // Identical interned types: only reject the genuinely non-equatable
            // shapes (struct/union/array/slice/void/fn), which have no equality.
            return !matches!(
                self.arena.get(a),
                Type::Struct(_)
                    | Type::Union(_)
                    | Type::Array { .. }
                    | Type::Slice { .. }
                    | Type::Void
                    | Type::Fn(_)
            );
        }
        if self.numeric(a) && self.numeric(b) {
            return self.try_unify_numeric(a, b).is_some();
        }
        match (self.arena.get(a), self.arena.get(b)) {
            // `opt == null` / `null == opt`: an optional compares against `null`
            // (the other side is `?deferred`). Two optionals of the SAME inner type
            // are already equatable via the `a == b` fast path.
            (Type::Optional(_), Type::Optional(inner))
            | (Type::Optional(inner), Type::Optional(_))
                if self.arena.is_bottom(*inner) =>
            {
                true
            }
            // Distinct enums of the SAME nominal identity were already caught by
            // the `a == b` fast path; two *different* enum ids are not equatable.
            (Type::Enum(x), Type::Enum(y)) => x == y,
            // Pointers compare for equality only when they point at the same
            // pointee; const-ness may differ (you may compare `*T` and `*const T`).
            (Type::Pointer { pointee: p, .. }, Type::Pointer { pointee: q, .. }) => p == q,
            // Error-set equality is meaningful only when the sets could share a
            // member: a distinct, disjoint pair (`error.Foo == error.Bar`) can
            // never be equal, so it is rejected. `anyerror` overlaps everything.
            (Type::ErrorSet(x), Type::ErrorSet(y)) => self.arena.errsets_overlap(*x, *y),
            (Type::ErrorSet(_), Type::AnyError) | (Type::AnyError, Type::ErrorSet(_)) => true,
            _ => false,
        }
    }

    /// Attempts to fold `e` into a statically-known `comptime_int` value. Folds
    /// integer literals, `-`/`~`-free `const` bindings whose value already folded
    /// to a comptime_int, and `+`/`-`/`*` plus unary `-` over folded operands.
    /// Returns `None` for anything not statically knowable here (a sized-int
    /// value, a `var`, a call, a non-foldable operator) — those keep the existing
    /// conservative behavior rather than risk a false "overflow". An arithmetic
    /// result that itself overflows `i128` also yields `None` (we do not claim a
    /// known value we cannot represent).
    pub(crate) fn fold_comptime_int(&self, e: &Expr) -> Option<i128> {
        match e {
            Expr::Int { text, base, .. } => crate::eval::parse_int_literal(text, *base),
            Expr::Comptime { inner, .. } => self.fold_comptime_int(inner),
            Expr::Ident { span, .. } => {
                if let Some(k2_resolve::Resolution::Def(id)) = self.resolution_at(*span) {
                    self.comptime_int_values.get(&id).copied()
                } else {
                    None
                }
            }
            Expr::Unary {
                op: UnOp::Neg,
                operand,
                ..
            } => self.fold_comptime_int(operand)?.checked_neg(),
            Expr::Binary { op, lhs, rhs, .. } => {
                let a = self.fold_comptime_int(lhs)?;
                let b = self.fold_comptime_int(rhs)?;
                match op {
                    BinOp::Add => a.checked_add(b),
                    BinOp::Sub => a.checked_sub(b),
                    BinOp::Mul => a.checked_mul(b),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Returns whichever of `a`/`b` is a bottom (prefer Deferred over Error).
    fn bottom_of(&self, a: TypeId, b: TypeId) -> TypeId {
        for t in [a, b] {
            if matches!(self.arena.get(t), Type::Deferred | Type::AnyType) {
                return self.arena.t_deferred();
            }
        }
        for t in [a, b] {
            if matches!(self.arena.get(t), Type::Error) {
                return self.arena.t_error();
            }
        }
        a
    }

    /// Unary operator typing.
    fn synth_unary(&mut self, op: UnOp, operand: &Expr, span: Span) -> TypeId {
        match op {
            UnOp::Not => {
                let bool_t = self.arena.t_bool();
                let t = self.synth(operand);
                if !self.arena.is_bottom(t) && !self.arena.coerces(t, bool_t) {
                    self.error(
                        span,
                        format!(
                            "operator `not` requires `bool`, found `{}`",
                            self.arena.fmt(t)
                        ),
                    );
                }
                bool_t
            }
            UnOp::Neg => {
                let t = self.synth(operand);
                if self.arena.is_bottom(t) {
                    return t;
                }
                if self.numeric(t) {
                    t
                } else {
                    self.error(
                        span,
                        format!(
                            "operator `-` requires a numeric type, found `{}`",
                            self.arena.fmt(t)
                        ),
                    );
                    self.arena.t_error()
                }
            }
            UnOp::BitNot => {
                let t = self.synth(operand);
                if self.arena.is_bottom(t) {
                    return t;
                }
                if self.integral(t) {
                    t
                } else {
                    self.error(
                        span,
                        format!(
                            "operator `~` requires an integer, found `{}`",
                            self.arena.fmt(t)
                        ),
                    );
                    self.arena.t_error()
                }
            }
            UnOp::AddrOf => {
                // `&.{}` / `&.{...}` (address of an anonymous initializer) is the
                // canonical empty-slice / aggregate-pointer literal; its concrete
                // shape is comptime, so it is Deferred and coerces to any slice or
                // pointer target (`.items = &.{}` in a generic struct).
                if matches!(operand, Expr::Init { ty: None, .. }) {
                    self.synth(operand);
                    return self.arena.t_deferred();
                }
                // `&e` -> `*T`. `&arr` -> `*[N]T` (the array type is the pointee).
                let t = self.synth(operand);
                self.arena.ptr(false, t)
            }
            UnOp::Try => self.synth_try(operand, span),
        }
    }

    /// `try e` — propagate the error of an error-union operand; result is its ok
    /// payload. The enclosing function must return an error union.
    fn synth_try(&mut self, operand: &Expr, span: Span) -> TypeId {
        let t = self.synth(operand);
        // Enclosing-fn check: the current function must return an error union.
        let in_eu = self
            .fn_stack
            .last()
            .map(|f| f.ret_is_error_union)
            .unwrap_or(false);
        if !in_eu {
            let ret = self
                .fn_stack
                .last()
                .map(|f| f.ret)
                .unwrap_or_else(|| self.arena.t_void());
            self.error(
                span,
                format!(
                    "`try` requires the enclosing function to return an error union; it returns `{}`",
                    self.arena.fmt(ret)
                ),
            );
        }
        if self.arena.is_bottom(t) {
            return self.arena.t_deferred();
        }
        match self.arena.get(t).clone() {
            Type::ErrorUnion { ok, .. } => ok,
            // `try` on a concrete, non-error-union value is a type error (spec §6.3:
            // the operand must be of some error-union type `E!T`). The Deferred /
            // generic-call path is taken by the `is_bottom` early-return above, so
            // only genuinely concrete operands reach here — `catch` already rejects
            // these symmetrically.
            _ => {
                self.error(
                    span,
                    format!(
                        "`try` requires an error union, found `{}`",
                        self.arena.fmt(t)
                    ),
                );
                self.arena.t_error()
            }
        }
    }

    // =====================================================================
    //  catch
    // =====================================================================

    /// `lhs catch [|err|] rhs` — handle an error union.
    fn synth_catch(
        &mut self,
        lhs: &Expr,
        capture: Option<&str>,
        rhs: &Expr,
        span: Span,
        _expected: Option<TypeId>,
    ) -> TypeId {
        let lt = self.synth(lhs);
        // Bind the capture (if any) to the error set, recorded at `rhs.span()`.
        if capture.is_some() {
            let err_ty = self.catch_err_type(lt);
            if let Some(def) = self.def_of(rhs.span()) {
                self.binding_types.insert(def, err_ty);
            }
        }
        if self.arena.is_bottom(lt) {
            self.synth(rhs);
            return self.arena.t_deferred();
        }
        match self.arena.get(lt).clone() {
            Type::ErrorUnion { ok, .. } => {
                // The fallback must produce `ok` or diverge.
                self.check(rhs, ok);
                ok
            }
            _ => {
                self.synth(rhs);
                self.error(
                    span,
                    format!(
                        "`catch` requires an error union, found `{}`",
                        self.arena.fmt(lt)
                    ),
                );
                self.arena.t_error()
            }
        }
    }

    /// The error-set type captured by `catch |err|` over an error-union lhs.
    fn catch_err_type(&mut self, lt: TypeId) -> TypeId {
        match self.arena.get(lt).clone() {
            Type::ErrorUnion {
                err: ErrSetRef::Set(id),
                ..
            } => self.arena.intern(Type::ErrorSet(id)),
            _ => self.arena.t_anyerror(),
        }
    }

    // =====================================================================
    //  Initializers
    // =====================================================================

    /// A typed or anonymous initializer `T{...}` / `.{...}`.
    fn synth_init(&mut self, ty: Option<&Expr>, body: &InitBody, span: Span) -> TypeId {
        match ty {
            Some(ty) => {
                let t = self.eval_type(ty);
                // Check fields/elements against the struct/array type.
                let _ = self.check_anon_init(body, span, t);
                t
            }
            None => {
                // Anonymous init with no expectation: synth values, stay Deferred.
                self.synth_init_body(body);
                self.arena.t_deferred()
            }
        }
    }

    // =====================================================================
    //  Control flow
    // =====================================================================

    /// A labeled/bare block expression. A block that ends by diverging (a
    /// `return`/`continue`/`break`/`unreachable`/`@panic`) has type `noreturn`,
    /// so it satisfies any expectation (e.g. a `catch |e| { ...; return; }`
    /// fallback). Otherwise it yields `void` (value-carrying `break :lbl v`
    /// blocks are conservatively `void` in v0.5).
    fn synth_block(
        &mut self,
        _label: Option<&str>,
        body: &[Stmtish],
        _span: Span,
        _expected: Option<TypeId>,
    ) -> TypeId {
        for s in body {
            self.check_stmt(s);
        }
        if body.last().map(stmt_diverges).unwrap_or(false) {
            self.arena.t_noreturn()
        } else {
            self.arena.t_void()
        }
    }

    /// An `if` expression/statement: bool/optional/error-union condition, then a
    /// join of the branches.
    #[allow(clippy::too_many_arguments)]
    fn synth_if(
        &mut self,
        cond: &Expr,
        capture: Option<&k2_syntax::Capture>,
        then_branch: &Expr,
        else_capture: Option<&k2_syntax::Capture>,
        else_branch: Option<&Expr>,
        _span: Span,
        expected: Option<TypeId>,
    ) -> TypeId {
        let payload = self.check_condition(cond, capture);
        self.bind_capture(capture, payload);
        let then_ty = match expected {
            Some(ex) => self.check(then_branch, ex),
            None => self.synth(then_branch),
        };
        if let Some(eb) = else_branch {
            // The else capture (error path) binds to the error set.
            if else_capture.is_some() {
                let err_ty = self.if_else_err_type(cond);
                self.bind_capture(else_capture, err_ty);
            }
            let else_ty = match expected {
                Some(ex) => self.check(eb, ex),
                None => self.synth(eb),
            };
            self.join(then_ty, else_ty, _span, expected)
        } else {
            // Statement `if` with no else: result is void.
            self.arena.t_void()
        }
    }

    /// Types the condition of an `if`/`while`, returning the captured payload
    /// type (for `|v|`): `bool` conditions yield void payload, optionals/error
    /// unions yield their unwrapped value.
    fn check_condition(&mut self, cond: &Expr, capture: Option<&k2_syntax::Capture>) -> TypeId {
        if capture.is_some() {
            // Optional/error-union capture form: do not require bool.
            let ct = self.synth(cond);
            if self.arena.is_bottom(ct) {
                return self.arena.t_deferred();
            }
            return match self.arena.get(ct).clone() {
                Type::Optional(inner) => inner,
                Type::ErrorUnion { ok, .. } => ok,
                _ => self.arena.t_deferred(),
            };
        }
        // Plain bool condition.
        let bool_t = self.arena.t_bool();
        let ct = self.synth(cond);
        if !self.arena.is_bottom(ct) && !self.arena.coerces(ct, bool_t) {
            self.error(
                cond.span(),
                format!("condition must be `bool`, found `{}`", self.arena.fmt(ct)),
            );
        }
        self.arena.t_void()
    }

    /// The error-set type captured by an `if (eu) |v| else |err|` clause.
    fn if_else_err_type(&mut self, cond: &Expr) -> TypeId {
        let ct = self.synth(cond);
        match self.arena.get(ct).clone() {
            Type::ErrorUnion {
                err: ErrSetRef::Set(id),
                ..
            } => self.arena.intern(Type::ErrorSet(id)),
            _ => self.arena.t_anyerror(),
        }
    }

    /// A `while` expression/statement.
    #[allow(clippy::too_many_arguments)]
    fn synth_while(
        &mut self,
        cond: &Expr,
        capture: Option<&k2_syntax::Capture>,
        cont: Option<&Stmtish>,
        body: &Expr,
        else_capture: Option<&k2_syntax::Capture>,
        else_branch: Option<&Expr>,
    ) -> TypeId {
        let payload = self.check_condition(cond, capture);
        self.bind_capture(capture, payload);
        if let Some(cont) = cont {
            self.check_stmt(cont);
        }
        self.synth(body);
        if else_capture.is_some() {
            let err_ty = self.if_else_err_type(cond);
            self.bind_capture(else_capture, err_ty);
        }
        if let Some(eb) = else_branch {
            self.synth(eb);
        }
        self.arena.t_void()
    }

    /// A `for` expression/statement.
    fn synth_for(
        &mut self,
        operands: &[ForOperand],
        captures: &[k2_syntax::CaptureName],
        body: &Expr,
        else_branch: Option<&Expr>,
    ) -> TypeId {
        self.bind_for_captures(operands, captures);
        self.synth(body);
        if let Some(eb) = else_branch {
            self.synth(eb);
        }
        self.arena.t_void()
    }

    /// Joins two branch result types (see [`Self::join`]).
    pub(crate) fn join(
        &mut self,
        a: TypeId,
        b: TypeId,
        span: Span,
        expected: Option<TypeId>,
    ) -> TypeId {
        if matches!(self.arena.get(a), Type::NoReturn) {
            return b;
        }
        if matches!(self.arena.get(b), Type::NoReturn) {
            return a;
        }
        if self.arena.is_bottom(a) || self.arena.is_bottom(b) {
            return self.bottom_of(a, b);
        }
        if a == b {
            return a;
        }
        if let Some(common) = self.try_unify_numeric(a, b) {
            return common;
        }
        // If an outer expectation exists, defer the mismatch to that check.
        if expected.is_some() {
            return a;
        }
        self.error(
            span,
            format!(
                "branches have incompatible types `{}` and `{}`",
                self.arena.fmt(a),
                self.arena.fmt(b)
            ),
        );
        self.arena.t_error()
    }

    // =====================================================================
    //  depth guard (expression)
    // =====================================================================

    fn enter_expr(&mut self) -> bool {
        // Reuse the statement depth guard by delegating to a small counter.
        self.expr_enter()
    }

    fn leave_expr(&mut self) {
        self.expr_leave();
    }
}

/// A type alias so the control-flow signatures read naturally; the AST stores
/// continuation/body as `Stmt`.
type Stmtish = k2_syntax::Stmt;

/// A short word for an arithmetic binary operator (for messages).
fn binop_word(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        _ => "?",
    }
}

/// A display name for a call's callee (`add`, `<expr>`), for arity messages.
fn callee_name(callee: &Expr) -> String {
    match callee {
        Expr::Ident { name, .. } => format!("`{name}`"),
        Expr::Field { field, .. } => format!("`{field}`"),
        _ => "value".to_string(),
    }
}

/// `true` if a statement unconditionally diverges (transfers control away), so a
/// block ending in it never falls through and has type `noreturn`.
fn stmt_diverges(s: &Stmtish) -> bool {
    match s {
        k2_syntax::Stmt::Return { .. }
        | k2_syntax::Stmt::Break { .. }
        | k2_syntax::Stmt::Continue { .. } => true,
        k2_syntax::Stmt::Expr { expr, .. } => matches!(expr, Expr::Unreachable { .. }),
        _ => false,
    }
}
