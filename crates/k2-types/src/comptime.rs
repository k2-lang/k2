//! The sandboxed comptime evaluator: `eval_expr` / `eval_stmt` over ordinary k2
//! expressions and statements, with a scope [`Env`], a fuel budget, and the
//! non-local [`Diverge`] outcomes.
//!
//! This is the heart of the v0.6 Sema. Where the v0.5 checker produced
//! [`Type::Deferred`](crate::ty::Type::Deferred) at a comptime boundary, the
//! checker now first tries to *evaluate* the operands here; only if they are
//! genuinely runtime-known (or reach an opaque std/`sys` namespace) does it stay
//! deferred. The engine shares the [`Checker`](crate::check::Checker)'s arena
//! and resolver, so it can produce real [`TypeId`]s (generic instantiation,
//! `@Type`) without a separate crate that would cycle on the arena.
//!
//! ## Sandbox & termination (spec §07.10)
//!
//! Comptime evaluation is pure: no I/O, no runtime allocation. Comptime data
//! lives in the compiler's own `Vec`/`Box`s. Termination is *guaranteed* by a
//! fuel budget ([`COMPTIME_FUEL`]): every `eval_expr`/`eval_stmt` entry and
//! every loop back-edge / call burns one unit, and exhaustion emits the
//! spec-mandated `"comptime evaluation exceeded N steps"` diagnostic and aborts
//! (it never hangs).
//!
//! ## The fallback contract
//!
//! The checker integration depends on one invariant: an
//! [`Err(Diverge::NotComptime)`] means "this operand is not comptime-known —
//! stay [`Deferred`](crate::ty::Type::Deferred)" and carries *no* diagnostic;
//! every *other* `Err` has already queued a diagnostic and means "fall back to
//! [`Error`](crate::ty::Type::Error)". This is what keeps std/`sys`/`build`
//! members opaque while user generics and reflection become concrete.

use std::collections::HashMap;

use k2_resolve::{DefId, Resolution};
use k2_syntax::{AssignOp, BinOp, Expr, ForOperand, InitBody, Stmt, UnOp};

use crate::value::{ComptimeInt, Value};

/// The fuel budget for a single top-level comptime evaluation: the maximum
/// number of evaluation steps (expression/statement entries, loop back-edges,
/// and calls) before the evaluator gives up with a diagnostic. Generous enough
/// for the reflection/generics in the corpus, small enough that a runaway
/// metaprogram is reported in well under a second.
pub(crate) const COMPTIME_FUEL: u64 = 2_000_000;

/// A hard cap on the element/byte length of any single comptime aggregate
/// (string, array, tuple, struct field count). Constructing a value past this
/// is reported and stopped, so an exponential-growth metaprogram (`s = s ++ s`
/// in a loop) terminates with a diagnostic well before it can exhaust memory —
/// the fuel budget alone counts *steps*, not bytes, so a value that doubles in
/// size each step would otherwise OOM while burning O(1) fuel (spec §07.10.3).
pub(crate) const COMPTIME_MAX_VALUE_BYTES: usize = 1 << 24; // 16 MiB

/// A comptime scope stack. Each frame maps a binding's [`DefId`] (reused from
/// the resolver, so names are never re-resolved) to its comptime [`Value`].
pub(crate) struct Env {
    /// The innermost-last stack of name->value frames.
    scopes: Vec<HashMap<DefId, Value>>,
}

impl Env {
    /// A fresh environment with one (function/block) frame.
    pub(crate) fn new() -> Env {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    /// Looks up a binding, walking from the innermost frame outward.
    fn lookup(&self, id: DefId) -> Option<&Value> {
        self.scopes.iter().rev().find_map(|s| s.get(&id))
    }

    /// Defines a binding in the innermost frame.
    pub(crate) fn define(&mut self, id: DefId, v: Value) {
        if let Some(top) = self.scopes.last_mut() {
            top.insert(id, v);
        }
    }

    /// Mutates an existing binding (a comptime `var`); returns `false` if the
    /// name is not bound in any frame.
    fn assign(&mut self, id: DefId, v: Value) -> bool {
        for s in self.scopes.iter_mut().rev() {
            if let std::collections::hash_map::Entry::Occupied(mut e) = s.entry(id) {
                e.insert(v);
                return true;
            }
        }
        false
    }

    /// Iterates every `(DefId, Value)` binding across all frames, innermost-last.
    /// Used to re-bind a generic instantiation's comptime params (`T`) into the
    /// checker's `binding_types`/`item_types` when re-checking method bodies.
    pub(crate) fn iter_bindings(&self) -> impl Iterator<Item = (DefId, Value)> + '_ {
        self.scopes
            .iter()
            .flat_map(|s| s.iter().map(|(k, v)| (*k, v.clone())))
    }

    /// Pushes a nested scope frame (a block / loop body).
    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Pops the innermost scope frame.
    fn pop(&mut self) {
        self.scopes.pop();
    }
}

impl Default for Env {
    fn default() -> Env {
        Env::new()
    }
}

/// Why a comptime evaluation stopped non-locally.
///
/// Only [`Diverge::NotComptime`] means "no diagnostic; the checker should stay
/// [`Deferred`](crate::ty::Type::Deferred)". Every other variant either carries
/// a value to the nearest enclosing boundary (`Return`/`Break`/`Continue`) or
/// has already queued a diagnostic (`CompileError`/`Fuel`).
pub(crate) enum Diverge {
    /// `return e;` — unwinds to the enclosing call boundary, carrying its value.
    Return(Value),
    /// `break [v]` — unwinds to the enclosing loop/block boundary. The v0.6
    /// engine treats every `break`/`continue` as targeting the innermost loop
    /// (no labeled-loop targeting at comptime yet), which suffices for the
    /// corpus and stays sound: a label is ignored, never mis-targeted.
    Break(Option<Value>),
    /// `continue` — restarts the enclosing loop.
    Continue,
    /// `@compileError(msg)` executed (or `@panic`/`unreachable`/div-by-zero/
    /// overflow): a diagnostic was queued; abort this evaluation.
    CompileError,
    /// Fuel exhausted: the budget diagnostic was queued; abort.
    Fuel,
    /// The operand is genuinely runtime-only / not comptime-known (a runtime
    /// `var`, an `@import` member, an `sys`/allocator access). No diagnostic;
    /// the checker stays [`Deferred`](crate::ty::Type::Deferred).
    NotComptime,
}

/// Shorthand for an evaluation result.
pub(crate) type EvalResult = Result<Value, Diverge>;

impl crate::check::Checker<'_> {
    // =====================================================================
    //  Fuel
    // =====================================================================

    /// Burns one unit of comptime fuel; on exhaustion queues the budget
    /// diagnostic (once) and returns [`Diverge::Fuel`].
    pub(crate) fn burn(&mut self, span: k2_syntax::Span) -> Result<(), Diverge> {
        if self.comptime_fuel == 0 {
            if !self.comptime_fuel_reported {
                self.comptime_fuel_reported = true;
                self.error(
                    span,
                    format!("comptime evaluation exceeded {COMPTIME_FUEL} steps"),
                );
            }
            return Err(Diverge::Fuel);
        }
        self.comptime_fuel -= 1;
        Ok(())
    }

    /// Resets the fuel budget for a new top-level evaluation.
    pub(crate) fn reset_fuel(&mut self) {
        self.comptime_fuel = COMPTIME_FUEL;
        self.comptime_fuel_reported = false;
    }

    /// Charges fuel proportional to the size of a constructed/cloned comptime
    /// aggregate of `len` elements (so a value that doubles each loop iteration
    /// burns its true cost in fuel, not O(1)), and rejects any aggregate larger
    /// than [`COMPTIME_MAX_VALUE_BYTES`] with the budget diagnostic. This is what
    /// makes `s ++ s` in a comptime loop terminate with a precise message instead
    /// of hanging/OOMing (spec §07.10.3).
    pub(crate) fn charge_aggregate(
        &mut self,
        len: usize,
        span: k2_syntax::Span,
    ) -> Result<(), Diverge> {
        if len > COMPTIME_MAX_VALUE_BYTES {
            if !self.comptime_fuel_reported {
                self.comptime_fuel_reported = true;
                self.error(
                    span,
                    format!(
                        "comptime value exceeded the {COMPTIME_MAX_VALUE_BYTES}-byte size limit"
                    ),
                );
            }
            return Err(Diverge::Fuel);
        }
        // Burn one unit per element so the fuel budget bounds total bytes moved.
        let units = len as u64;
        if self.comptime_fuel <= units {
            self.comptime_fuel = 0;
            return self.burn(span);
        }
        self.comptime_fuel -= units;
        Ok(())
    }

    /// Emits a compiler-discovered comptime error (div-by-zero, index OOB,
    /// `unreachable`/`@panic` reached), appending the *instantiation context*
    /// (`(in instantiation of `List(u32)`)`) when one is in progress, so a
    /// failure inside a monomorphized body names where it came from. A user
    /// `@compileError` message is emitted verbatim instead (spec §07.9.1).
    pub(crate) fn comptime_error(&mut self, span: k2_syntax::Span, message: impl Into<String>) {
        let mut msg = message.into();
        if let Some((fn_def, _)) = self.inst_stack.last() {
            let name = self.resolved.defs[fn_def.index()].name.clone();
            msg.push_str(&format!(" (in instantiation of `{name}`)"));
        }
        self.error(span, msg);
    }

    // =====================================================================
    //  Top-level entry points used by the checker integration
    // =====================================================================

    /// Evaluates `e` in a fresh comptime context, returning its [`Value`] only
    /// when fully comptime-known. A new fuel budget is installed per call (so a
    /// nested instantiation shares the budget of its driver, but each top-level
    /// boundary starts fresh).
    pub(crate) fn comptime_eval_value(&mut self, e: &Expr) -> Option<Value> {
        self.reset_fuel();
        let mut env = Env::new();
        self.eval_expr(&mut env, e).ok()
    }

    /// Evaluates `e` and, if it produced a `type` value, returns that
    /// [`TypeId`]. Used by the type-position boundaries.
    pub(crate) fn comptime_eval_type(&mut self, e: &Expr) -> Option<crate::ty::TypeId> {
        self.comptime_eval_value(e).and_then(|v| v.as_type())
    }

    // =====================================================================
    //  Expression evaluation
    // =====================================================================

    /// Evaluates an expression to a comptime [`Value`].
    pub(crate) fn eval_expr(&mut self, env: &mut Env, e: &Expr) -> EvalResult {
        self.burn(e.span())?;
        match e {
            // ---- Literals ----------------------------------------------
            Expr::Int { text, base, .. } => {
                let v = crate::eval::parse_int_literal(text, *base).ok_or(Diverge::NotComptime)?;
                let ty = self.arena.t_comptime_int();
                Ok(Value::Int(ComptimeInt { v, ty }))
            }
            Expr::Char { text, .. } => {
                let v = decode_char_literal(text).ok_or(Diverge::NotComptime)?;
                let ty = self.arena.t_comptime_int();
                Ok(Value::Int(ComptimeInt { v, ty }))
            }
            Expr::Float { text, .. } => {
                let v: f64 = text
                    .chars()
                    .filter(|c| *c != '_')
                    .collect::<String>()
                    .parse()
                    .map_err(|_| Diverge::NotComptime)?;
                let ty = self.arena.t_comptime_float();
                Ok(Value::Float { v, ty })
            }
            Expr::Str { text, .. } => Ok(Value::Str(decode_str_literal(text))),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::Undefined { .. } => Ok(Value::Undefined(self.arena.t_deferred())),
            Expr::Null { .. } => Err(Diverge::NotComptime),
            Expr::Unreachable { span } => {
                self.comptime_error(*span, "reached `unreachable` at comptime");
                Err(Diverge::CompileError)
            }

            // ---- Names -------------------------------------------------
            Expr::Ident { name, span } => self.eval_ident(env, name, *span),

            // ---- Type-constructor expressions (their value is a type) --
            Expr::Optional { .. }
            | Expr::Pointer { .. }
            | Expr::Slice { .. }
            | Expr::ManyPtr { .. }
            | Expr::ArrayType { .. }
            | Expr::ErrorUnion { .. }
            | Expr::FnType { .. }
            | Expr::ErrorSet { .. }
            | Expr::AnyType { .. }
            | Expr::Container(_) => {
                // A type expression evaluates to the type it denotes, with the
                // current comptime bindings (e.g. `[n]T`, `[]T`) in scope.
                let t = self.eval_type_comptime(env, e)?;
                Ok(Value::Type(t))
            }

            // ---- Operators ---------------------------------------------
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(env, *op, lhs, rhs, *span),
            Expr::Unary { op, operand, span } => self.eval_unary(env, *op, operand, *span),

            // ---- comptime ----------------------------------------------
            Expr::Comptime { inner, .. } => self.eval_expr(env, inner),

            // ---- Field / index / unwrap / deref ------------------------
            Expr::Field { base, field, span } => self.eval_field(env, base, field, *span),
            Expr::Index { base, index, span } => self.eval_index(env, base, index, *span),
            Expr::Unwrap { base, .. } => self.eval_expr(env, base),
            Expr::Deref { base, .. } => self.eval_expr(env, base),

            // ---- Initializers ------------------------------------------
            Expr::Init { ty, body, span } => self.eval_init(env, ty.as_deref(), body, *span),

            // ---- Calls / builtins --------------------------------------
            Expr::Call { callee, args, span } => self.eval_call(env, callee, args, *span),
            Expr::Builtin { name, args, span } => self.eval_builtin_value(env, name, args, *span),

            // ---- Enum / error literals ---------------------------------
            Expr::EnumLiteral { name, span } => self.eval_enum_literal(name, *span),
            Expr::ErrorLiteral { name, .. } => {
                let id = self.arena.intern_errset(vec![name.clone()]);
                let set = self.arena.intern(crate::ty::Type::ErrorSet(id));
                Ok(Value::ErrVal {
                    set,
                    name: name.clone(),
                })
            }

            // ---- Control flow ------------------------------------------
            Expr::If {
                cond,
                capture,
                then_branch,
                else_branch,
                ..
            } => self.eval_if(
                env,
                cond,
                capture.as_ref(),
                then_branch,
                else_branch.as_deref(),
            ),
            Expr::Block { body, span, .. } => self.eval_block(env, body, *span),
            Expr::While {
                cond,
                cont,
                body,
                span,
                ..
            } => self.eval_while(env, cond, cont.as_deref(), body, *span),
            Expr::For {
                operands,
                captures,
                body,
                span,
                ..
            } => self.eval_for(env, operands, captures, body, *span),
            Expr::Switch {
                scrutinee,
                arms,
                span,
            } => self.eval_switch(env, scrutinee, arms, *span),
            Expr::Catch { lhs, .. } => self.eval_expr(env, lhs),

            // The remaining forms (slice exprs, etc.) are not comptime-modelled.
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Resolves an identifier to a comptime value: an in-scope comptime binding,
    /// a comptime-known global `const`, or a type-denoting name.
    fn eval_ident(&mut self, env: &mut Env, name: &str, span: k2_syntax::Span) -> EvalResult {
        match self.resolution_at(span) {
            Some(Resolution::Predeclared(_)) => {
                // A predeclared type name denotes a `type` value.
                let t = self.predeclared_type(name);
                if matches!(self.arena.get(t), crate::ty::Type::Deferred) {
                    Err(Diverge::NotComptime)
                } else {
                    Ok(Value::Type(t))
                }
            }
            Some(Resolution::Def(id)) => {
                if let Some(v) = env.lookup(id) {
                    return Ok(v.clone());
                }
                // A comptime-known global value (a folded const).
                if let Some(v) = self.comptime_const_values.get(&id) {
                    return Ok(v.clone());
                }
                // A type-denoting const (`const Int = i32;`).
                if let Some(&t) = self.item_types.get(&id) {
                    if !self.arena.is_bottom(t) {
                        return Ok(Value::Type(t));
                    }
                }
                // A const whose value folded to a comptime_int.
                if let Some(&v) = self.comptime_int_values.get(&id) {
                    let ty = self.arena.t_comptime_int();
                    return Ok(Value::Int(ComptimeInt { v, ty }));
                }
                // A fn used as a value.
                if matches!(
                    self.resolved.defs[id.index()].kind,
                    k2_resolve::DefKind::Item
                ) && self.fn_items.contains_key(&id)
                {
                    return Ok(Value::Fn(id));
                }
                Err(Diverge::NotComptime)
            }
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Evaluates a binary operator over comptime values.
    fn eval_binary(
        &mut self,
        env: &mut Env,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: k2_syntax::Span,
    ) -> EvalResult {
        // Tag comparison: `info == .Struct` / `info != .Struct`. One side is a
        // bare enum/tag literal naming a variant of the other's union/enum type;
        // compare by variant name without forcing the literal to a value.
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            if let Some(b) = self.eval_tag_comparison(env, op, lhs, rhs)? {
                return Ok(b);
            }
        }

        // Short-circuit logical ops.
        match op {
            BinOp::And => {
                let l = self
                    .eval_expr(env, lhs)?
                    .as_bool()
                    .ok_or(Diverge::NotComptime)?;
                if !l {
                    return Ok(Value::Bool(false));
                }
                let r = self
                    .eval_expr(env, rhs)?
                    .as_bool()
                    .ok_or(Diverge::NotComptime)?;
                return Ok(Value::Bool(r));
            }
            BinOp::Or => {
                let l = self
                    .eval_expr(env, lhs)?
                    .as_bool()
                    .ok_or(Diverge::NotComptime)?;
                if l {
                    return Ok(Value::Bool(true));
                }
                let r = self
                    .eval_expr(env, rhs)?
                    .as_bool()
                    .ok_or(Diverge::NotComptime)?;
                return Ok(Value::Bool(r));
            }
            _ => {}
        }

        let l = self.eval_expr(env, lhs)?;
        let r = self.eval_expr(env, rhs)?;

        // `++` string/array concatenation.
        if matches!(op, BinOp::Concat) {
            return self.eval_concat(&l, &r, span);
        }

        // Type equality (`i32 == u32`, `info.tag == ...`).
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            if let (Value::Type(a), Value::Type(b)) = (&l, &r) {
                let eq = a == b;
                return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
            }
            if let (Value::Bool(a), Value::Bool(b)) = (&l, &r) {
                let eq = a == b;
                return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
            }
            if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
                let eq = a == b;
                return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
            }
            // Enum equality (an `@typeInfo` tag vs an `.Int`-style literal).
            if let (Value::Enum { which: a, .. }, Value::Enum { which: b, .. }) = (&l, &r) {
                let eq = a == b;
                return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
            }
        }

        // Float arithmetic / comparison.
        if let (Some(lf), Some(rf)) = (as_float(&l), as_float(&r)) {
            return self.eval_float_binop(op, lf, rf, span);
        }

        // Integer arithmetic / comparison / bitwise.
        let (la, ra) = (l.as_int(), r.as_int());
        if let (Some(a), Some(b)) = (la, ra) {
            return self.eval_int_binop(op, a, b, span);
        }

        Err(Diverge::NotComptime)
    }

    /// Handles a `==`/`!=` whose one operand is a bare `.Variant` tag literal and
    /// whose other operand is a union/enum value (the `info == .Struct` idiom).
    /// Returns `Ok(Some(bool))` on a tag comparison, `Ok(None)` otherwise.
    fn eval_tag_comparison(
        &mut self,
        env: &mut Env,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<Option<Value>, Diverge> {
        let (tag_name, other) = match (as_tag_literal(lhs), as_tag_literal(rhs)) {
            (Some(n), _) => (n, rhs),
            (_, Some(n)) => (n, lhs),
            _ => return Ok(None),
        };
        let v = match self.eval_expr(env, other) {
            Ok(v) => v,
            Err(Diverge::NotComptime) => return Ok(None),
            Err(other) => return Err(other),
        };
        let active = match &v {
            Value::Union { ty, which, .. } | Value::Enum { ty, which } => {
                self.variant_name(*ty, *which)
            }
            _ => return Ok(None),
        };
        let Some(active) = active else {
            return Ok(None);
        };
        let eq = active == tag_name;
        Ok(Some(Value::Bool(if matches!(op, BinOp::Eq) {
            eq
        } else {
            !eq
        })))
    }

    /// The variant name at index `which` of an enum/union type, if known.
    fn variant_name(&self, ty: crate::ty::TypeId, which: u32) -> Option<String> {
        match self.arena.get(ty) {
            crate::ty::Type::Enum(id) => self.arena.enums[id.0 as usize]
                .variants
                .get(which as usize)
                .map(|v| v.name.clone()),
            crate::ty::Type::Union(id) => self.arena.unions[id.0 as usize]
                .variants
                .get(which as usize)
                .map(|v| v.name.clone()),
            _ => None,
        }
    }

    /// Integer binary-op evaluation with overflow / div-by-zero diagnostics.
    fn eval_int_binop(&mut self, op: BinOp, a: i128, b: i128, span: k2_syntax::Span) -> EvalResult {
        let ci = self.arena.t_comptime_int();
        let int = |v: i128| Value::Int(ComptimeInt { v, ty: ci });
        let bool_v = |b: bool| Value::Bool(b);
        match op {
            BinOp::Add => a.checked_add(b).map(int).ok_or(Diverge::NotComptime),
            BinOp::Sub => a.checked_sub(b).map(int).ok_or(Diverge::NotComptime),
            BinOp::Mul => a.checked_mul(b).map(int).ok_or(Diverge::NotComptime),
            BinOp::Div => {
                if b == 0 {
                    self.comptime_error(span, "comptime division by zero");
                    return Err(Diverge::CompileError);
                }
                a.checked_div(b).map(int).ok_or(Diverge::NotComptime)
            }
            BinOp::Rem => {
                if b == 0 {
                    self.comptime_error(span, "comptime remainder by zero");
                    return Err(Diverge::CompileError);
                }
                a.checked_rem(b).map(int).ok_or(Diverge::NotComptime)
            }
            BinOp::BitAnd => Ok(int(a & b)),
            BinOp::BitOr => Ok(int(a | b)),
            BinOp::BitXor => Ok(int(a ^ b)),
            BinOp::Shl => {
                if !(0..128).contains(&b) {
                    return Err(Diverge::NotComptime);
                }
                a.checked_shl(b as u32).map(int).ok_or(Diverge::NotComptime)
            }
            BinOp::Shr => {
                if !(0..128).contains(&b) {
                    return Err(Diverge::NotComptime);
                }
                Ok(int(a >> (b as u32)))
            }
            BinOp::Eq => Ok(bool_v(a == b)),
            BinOp::Ne => Ok(bool_v(a != b)),
            BinOp::Lt => Ok(bool_v(a < b)),
            BinOp::Le => Ok(bool_v(a <= b)),
            BinOp::Gt => Ok(bool_v(a > b)),
            BinOp::Ge => Ok(bool_v(a >= b)),
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Float binary-op evaluation.
    fn eval_float_binop(
        &mut self,
        op: BinOp,
        a: f64,
        b: f64,
        _span: k2_syntax::Span,
    ) -> EvalResult {
        let cf = self.arena.t_comptime_float();
        let float = |v: f64| Value::Float { v, ty: cf };
        let bool_v = |b: bool| Value::Bool(b);
        match op {
            BinOp::Add => Ok(float(a + b)),
            BinOp::Sub => Ok(float(a - b)),
            BinOp::Mul => Ok(float(a * b)),
            BinOp::Div => Ok(float(a / b)),
            BinOp::Eq => Ok(bool_v(a == b)),
            BinOp::Ne => Ok(bool_v(a != b)),
            BinOp::Lt => Ok(bool_v(a < b)),
            BinOp::Le => Ok(bool_v(a <= b)),
            BinOp::Gt => Ok(bool_v(a > b)),
            BinOp::Ge => Ok(bool_v(a >= b)),
            _ => Err(Diverge::NotComptime),
        }
    }

    /// `++` over two strings or two arrays/tuples. Fuel is charged proportional
    /// to the *result* length (and the result size is capped), so a doubling
    /// `s ++ s` loop terminates with a diagnostic instead of OOMing.
    fn eval_concat(&mut self, l: &Value, r: &Value, span: k2_syntax::Span) -> EvalResult {
        match (l, r) {
            (Value::Str(a), Value::Str(b)) => {
                self.charge_aggregate(a.len() + b.len(), span)?;
                Ok(Value::Str(format!("{a}{b}")))
            }
            (Value::Array { elems: a, .. }, Value::Array { elems: b, .. }) => {
                self.charge_aggregate(a.len() + b.len(), span)?;
                let mut elems = a.clone();
                elems.extend(b.iter().cloned());
                let ty = self.arena.t_deferred();
                Ok(Value::Array { ty, elems })
            }
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Unary operator evaluation.
    fn eval_unary(
        &mut self,
        env: &mut Env,
        op: UnOp,
        operand: &Expr,
        _span: k2_syntax::Span,
    ) -> EvalResult {
        match op {
            UnOp::Neg => {
                let v = self.eval_expr(env, operand)?;
                match v {
                    Value::Int(ci) => {
                        let n = ci.v.checked_neg().ok_or(Diverge::NotComptime)?;
                        Ok(Value::Int(ComptimeInt { v: n, ty: ci.ty }))
                    }
                    Value::Float { v, ty } => Ok(Value::Float { v: -v, ty }),
                    _ => Err(Diverge::NotComptime),
                }
            }
            UnOp::BitNot => {
                let v = self.eval_expr(env, operand)?;
                match v {
                    Value::Int(ci) => Ok(Value::Int(ComptimeInt {
                        v: !ci.v,
                        ty: ci.ty,
                    })),
                    _ => Err(Diverge::NotComptime),
                }
            }
            UnOp::Not => {
                let b = self
                    .eval_expr(env, operand)?
                    .as_bool()
                    .ok_or(Diverge::NotComptime)?;
                Ok(Value::Bool(!b))
            }
            // `&.{...}` — address-of an anonymous initializer becomes the
            // (comptime) sequence/slice itself.
            UnOp::AddrOf => self.eval_expr(env, operand),
            // `try` of a comptime error union is not modelled.
            UnOp::Try => Err(Diverge::NotComptime),
        }
    }

    /// Evaluates an anonymous/typed initializer into a struct/array/tuple value.
    fn eval_init(
        &mut self,
        env: &mut Env,
        ty: Option<&Expr>,
        body: &InitBody,
        _span: k2_syntax::Span,
    ) -> EvalResult {
        let declared = match ty {
            Some(t) => Some(self.eval_type_comptime(env, t)?),
            None => None,
        };
        match body {
            InitBody::Tuple(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    self.charge_aggregate(1, _span)?;
                    vals.push(self.eval_expr(env, e)?);
                }
                match declared {
                    Some(d) => Ok(Value::Array { ty: d, elems: vals }),
                    None => Ok(Value::Tuple(vals)),
                }
            }
            InitBody::Fields(fields) => {
                // A `.{ .name = v }` named init: build a struct value, ordering
                // fields by the declared struct's layout when known, else by the
                // written order (carried as a synthetic struct over Deferred).
                match declared {
                    Some(d) => self.build_struct_value(env, d, fields),
                    None => {
                        // Reflection descriptors (`.{ .Int = .{...} }`) are
                        // anonymous; model them as a single-key tuple-of-fields
                        // captured as an Array of name/value via a synthetic
                        // tagged value handled by @Type. We keep field order.
                        let mut vals = Vec::with_capacity(fields.len());
                        for f in fields {
                            self.charge_aggregate(1, _span)?;
                            vals.push((f.name.clone(), self.eval_expr(env, &f.value)?));
                        }
                        Ok(self.anon_struct_value(vals))
                    }
                }
            }
        }
    }

    /// Builds an anonymous value from named `(field, value)` pairs: a single key
    /// becomes an [`Value::AnonTagged`] (the `@Type(.{ .Int = ... })` shape);
    /// multiple keys become an [`Value::AnonStruct`].
    fn anon_struct_value(&self, pairs: Vec<(String, Value)>) -> Value {
        if pairs.len() == 1 {
            let (tag, payload) = pairs.into_iter().next().expect("one pair");
            Value::AnonTagged {
                tag,
                payload: Box::new(payload),
            }
        } else {
            Value::AnonStruct(pairs)
        }
    }

    /// Builds a struct [`Value`] for a typed initializer, slotting each named
    /// field into its declared layout position.
    fn build_struct_value(
        &mut self,
        env: &mut Env,
        struct_ty: crate::ty::TypeId,
        fields: &[k2_syntax::FieldInit],
    ) -> EvalResult {
        if let crate::ty::Type::Struct(id) = self.arena.get(struct_ty).clone() {
            let order: Vec<String> = self.arena.structs[id.0 as usize]
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect();
            let mut slots: Vec<Option<Value>> = vec![None; order.len()];
            for f in fields {
                let v = self.eval_expr(env, &f.value)?;
                if let Some(idx) = order.iter().position(|n| *n == f.name) {
                    slots[idx] = Some(v);
                }
            }
            let mut out = Vec::with_capacity(order.len());
            for s in slots {
                out.push(s.unwrap_or(Value::Void));
            }
            return Ok(Value::Struct {
                ty: struct_ty,
                fields: out,
            });
        }
        Err(Diverge::NotComptime)
    }

    /// A comptime `if` (only the live branch is evaluated, so a `@compileError`
    /// in the dead branch does not fire — spec §07.9.1).
    fn eval_if(
        &mut self,
        env: &mut Env,
        cond: &Expr,
        capture: Option<&k2_syntax::Capture>,
        then_branch: &Expr,
        else_branch: Option<&Expr>,
    ) -> EvalResult {
        // Only a plain bool condition is comptime-modelled (optional/error
        // captures are runtime forms).
        if capture.is_some() {
            return Err(Diverge::NotComptime);
        }
        let c = self
            .eval_expr(env, cond)?
            .as_bool()
            .ok_or(Diverge::NotComptime)?;
        if c {
            self.eval_expr(env, then_branch)
        } else if let Some(eb) = else_branch {
            self.eval_expr(env, eb)
        } else {
            Ok(Value::Void)
        }
    }

    /// A comptime block: run its statements, yielding `break :blk v` or `void`.
    fn eval_block(&mut self, env: &mut Env, body: &[Stmt], span: k2_syntax::Span) -> EvalResult {
        env.push();
        let r = self.eval_block_inner(env, body, span);
        env.pop();
        r
    }

    /// The body of [`Self::eval_block`], factored so the scope frame is always
    /// popped.
    fn eval_block_inner(
        &mut self,
        env: &mut Env,
        body: &[Stmt],
        _span: k2_syntax::Span,
    ) -> EvalResult {
        for s in body {
            match self.eval_stmt(env, s) {
                Ok(()) => {}
                Err(Diverge::Break(v)) => return Ok(v.unwrap_or(Value::Void)),
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Void)
    }

    /// A comptime `while` (the back-edge burns fuel, guaranteeing termination).
    fn eval_while(
        &mut self,
        env: &mut Env,
        cond: &Expr,
        cont: Option<&Stmt>,
        body: &Expr,
        span: k2_syntax::Span,
    ) -> EvalResult {
        loop {
            self.burn(span)?;
            let c = self
                .eval_expr(env, cond)?
                .as_bool()
                .ok_or(Diverge::NotComptime)?;
            if !c {
                break;
            }
            match self.eval_expr(env, body) {
                Ok(_) => {}
                Err(Diverge::Break(_)) => break,
                Err(Diverge::Continue) => {}
                Err(other) => return Err(other),
            }
            if let Some(cont) = cont {
                self.eval_stmt(env, cont)?;
            }
        }
        Ok(Value::Void)
    }

    /// A comptime `for` / `inline for`: iterate the operand sequence(s), binding
    /// the capture(s) per element.
    fn eval_for(
        &mut self,
        env: &mut Env,
        operands: &[ForOperand],
        captures: &[k2_syntax::CaptureName],
        body: &Expr,
        span: k2_syntax::Span,
    ) -> EvalResult {
        // v0.6 comptime-models the single-operand value form and the index-range
        // form (the shapes the reflection examples use).
        let seqs = self.for_operand_sequences(env, operands)?;
        let len = seqs.iter().map(|s| s.len()).min().unwrap_or(0);
        for i in 0..len {
            self.burn(span)?;
            env.push();
            for (opi, cap) in captures.iter().enumerate() {
                if cap.name == "_" {
                    continue;
                }
                if let Some(seq) = seqs.get(opi) {
                    if let Some(def) = self.def_of(cap.span) {
                        env.define(def, seq[i].clone());
                    }
                }
            }
            let r = self.eval_expr(env, body);
            env.pop();
            match r {
                Ok(_) => {}
                Err(Diverge::Break(_)) => break,
                Err(Diverge::Continue) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Void)
    }

    /// Resolves the per-operand value sequences a `for` iterates.
    fn for_operand_sequences(
        &mut self,
        env: &mut Env,
        operands: &[ForOperand],
    ) -> Result<Vec<Vec<Value>>, Diverge> {
        let mut out = Vec::with_capacity(operands.len());
        for op in operands {
            match op {
                ForOperand::Value(e) => {
                    let v = self.eval_expr(env, e)?;
                    match v {
                        Value::Array { elems, .. } => out.push(elems),
                        Value::Tuple(elems) => out.push(elems),
                        _ => return Err(Diverge::NotComptime),
                    }
                }
                ForOperand::Range { lo, hi, .. } => {
                    let lo_v = self
                        .eval_expr(env, lo)?
                        .as_int()
                        .ok_or(Diverge::NotComptime)?;
                    let hi_v = match hi {
                        Some(h) => self
                            .eval_expr(env, h)?
                            .as_int()
                            .ok_or(Diverge::NotComptime)?,
                        None => return Err(Diverge::NotComptime),
                    };
                    let ty = self.arena.t_usize();
                    let mut seq = Vec::new();
                    let mut k = lo_v;
                    while k < hi_v {
                        self.burn(lo.span())?;
                        seq.push(Value::Int(ComptimeInt { v: k, ty }));
                        k += 1;
                    }
                    out.push(seq);
                }
            }
        }
        Ok(out)
    }

    /// A comptime `switch`: only the matching arm is evaluated.
    fn eval_switch(
        &mut self,
        env: &mut Env,
        scrutinee: &Expr,
        arms: &[k2_syntax::SwitchArm],
        _span: k2_syntax::Span,
    ) -> EvalResult {
        let s = self.eval_expr(env, scrutinee)?;
        for arm in arms {
            match &arm.pattern {
                k2_syntax::SwitchPattern::Else => return self.eval_expr(env, &arm.body),
                k2_syntax::SwitchPattern::Items(items) => {
                    for item in items {
                        if self.switch_item_matches(env, &s, item)? {
                            return self.eval_expr(env, &arm.body);
                        }
                    }
                }
            }
        }
        Ok(Value::Void)
    }

    /// `true` if the scrutinee value matches one `switch` item.
    fn switch_item_matches(
        &mut self,
        env: &mut Env,
        scrutinee: &Value,
        item: &k2_syntax::SwitchItem,
    ) -> Result<bool, Diverge> {
        let lo = self.eval_expr(env, &item.lo)?;
        if let Some(hi) = &item.hi {
            let hi = self.eval_expr(env, hi)?;
            if let (Some(s), Some(a), Some(b)) = (scrutinee.as_int(), lo.as_int(), hi.as_int()) {
                return Ok(s >= a && s <= b);
            }
            return Ok(false);
        }
        Ok(values_equal(scrutinee, &lo))
    }

    // =====================================================================
    //  Statement evaluation
    // =====================================================================

    /// Evaluates one comptime statement, mutating `env`.
    pub(crate) fn eval_stmt(&mut self, env: &mut Env, s: &Stmt) -> Result<(), Diverge> {
        self.burn(s.span())?;
        match s {
            Stmt::Const { value, span, .. } => {
                let v = self.eval_expr(env, value)?;
                if let Some(def) = self.def_of(*span) {
                    env.define(def, v);
                }
                Ok(())
            }
            Stmt::Var { value, span, .. } => {
                let v = match value {
                    Some(e) => self.eval_expr(env, e)?,
                    None => Value::Undefined(self.arena.t_deferred()),
                };
                if let Some(def) = self.def_of(*span) {
                    env.define(def, v);
                }
                Ok(())
            }
            Stmt::Assign {
                target, op, value, ..
            } => self.eval_assign(env, target, *op, value),
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.eval_expr(env, e)?,
                    None => Value::Void,
                };
                Err(Diverge::Return(v))
            }
            Stmt::Break { value, .. } => {
                let v = match value {
                    Some(e) => Some(self.eval_expr(env, e)?),
                    None => None,
                };
                Err(Diverge::Break(v))
            }
            Stmt::Continue { .. } => Err(Diverge::Continue),
            Stmt::Expr { expr, .. } => {
                self.eval_expr(env, expr)?;
                Ok(())
            }
            Stmt::Block { body, span } => {
                env.push();
                let r = (|| {
                    for s in body {
                        self.eval_stmt(env, s)?;
                    }
                    Ok(())
                })();
                env.pop();
                let _ = span;
                r
            }
            Stmt::Comptime { body, .. } => {
                env.push();
                let r = (|| {
                    for s in body {
                        self.eval_stmt(env, s)?;
                    }
                    Ok(())
                })();
                env.pop();
                r
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => {
                self.eval_expr(env, expr)?;
                Ok(())
            }
            // defer/errdefer are runtime cleanup; not comptime-modelled here.
            Stmt::Defer { .. } | Stmt::Errdefer { .. } => Err(Diverge::NotComptime),
        }
    }

    /// Evaluates an assignment to a comptime `var`, including `s[i] = v` and
    /// `obj.f = v` into a stored aggregate value.
    fn eval_assign(
        &mut self,
        env: &mut Env,
        target: &Expr,
        op: AssignOp,
        value: &Expr,
    ) -> Result<(), Diverge> {
        let rhs = self.eval_expr(env, value)?;
        match target {
            Expr::Ident { name, span } => {
                if name == "_" {
                    return Ok(());
                }
                let Some(Resolution::Def(id)) = self.resolution_at(*span) else {
                    return Err(Diverge::NotComptime);
                };
                let new = if matches!(op, AssignOp::Eq) {
                    rhs
                } else {
                    let cur = env.lookup(id).cloned().ok_or(Diverge::NotComptime)?;
                    self.apply_compound(op, &cur, &rhs)?
                };
                if env.assign(id, new) {
                    Ok(())
                } else {
                    Err(Diverge::NotComptime)
                }
            }
            // `arr[i] = v` into a comptime array stored in an ident.
            Expr::Index { base, index, .. } => {
                let Expr::Ident { span, .. } = base.as_ref() else {
                    return Err(Diverge::NotComptime);
                };
                let Some(Resolution::Def(id)) = self.resolution_at(*span) else {
                    return Err(Diverge::NotComptime);
                };
                let idx = self
                    .eval_expr(env, index)?
                    .as_int()
                    .ok_or(Diverge::NotComptime)?;
                let mut cur = env.lookup(id).cloned().ok_or(Diverge::NotComptime)?;
                if let Value::Array { elems, .. } = &mut cur {
                    let i = usize::try_from(idx).map_err(|_| Diverge::NotComptime)?;
                    if i >= elems.len() {
                        return Err(Diverge::NotComptime);
                    }
                    elems[i] = if matches!(op, AssignOp::Eq) {
                        rhs
                    } else {
                        let prev = elems[i].clone();
                        self.apply_compound(op, &prev, &rhs)?
                    };
                    env.assign(id, cur);
                    return Ok(());
                }
                Err(Diverge::NotComptime)
            }
            _ => Err(Diverge::NotComptime),
        }
    }

    /// Applies a compound assignment operator (`+=`, `<<=`, …) at comptime.
    fn apply_compound(&mut self, op: AssignOp, cur: &Value, rhs: &Value) -> EvalResult {
        let binop = match op {
            AssignOp::AddEq => BinOp::Add,
            AssignOp::SubEq => BinOp::Sub,
            AssignOp::MulEq => BinOp::Mul,
            AssignOp::DivEq => BinOp::Div,
            AssignOp::RemEq => BinOp::Rem,
            AssignOp::AndEq => BinOp::BitAnd,
            AssignOp::OrEq => BinOp::BitOr,
            AssignOp::XorEq => BinOp::BitXor,
            AssignOp::ShlEq => BinOp::Shl,
            AssignOp::ShrEq => BinOp::Shr,
            AssignOp::Eq => return Ok(rhs.clone()),
        };
        if let (Some(a), Some(b)) = (cur.as_int(), rhs.as_int()) {
            return self.eval_int_binop(binop, a, b, k2_syntax::Span::default());
        }
        Err(Diverge::NotComptime)
    }

    // =====================================================================
    //  Type evaluation under a comptime environment
    // =====================================================================

    /// Evaluates a type-position expression with the comptime environment in
    /// scope, so `[n]T`, `[]T`, `?T`, and a generic struct body see their bound
    /// `comptime` params. Falls back to the static `eval_type` for everything
    /// the env does not affect.
    pub(crate) fn eval_type_comptime(
        &mut self,
        env: &mut Env,
        e: &Expr,
    ) -> Result<crate::ty::TypeId, Diverge> {
        match e {
            Expr::Ident { span, .. } => {
                // A type param `T` bound in the env resolves to its Value::Type.
                if let Some(Resolution::Def(id)) = self.resolution_at(*span) {
                    if let Some(Value::Type(t)) = env.lookup(id) {
                        return Ok(*t);
                    }
                }
                Ok(self.eval_type(e))
            }
            Expr::Optional { inner, .. } => {
                let i = self.eval_type_comptime(env, inner)?;
                Ok(self.arena.optional(i))
            }
            Expr::Pointer {
                is_const, inner, ..
            } => {
                let p = self.eval_type_comptime(env, inner)?;
                Ok(self.arena.ptr(*is_const, p))
            }
            Expr::Slice {
                is_const, inner, ..
            } => {
                let el = self.eval_type_comptime(env, inner)?;
                Ok(self.arena.slice(*is_const, el))
            }
            Expr::ManyPtr {
                is_const, inner, ..
            } => {
                let pointee = self.eval_type_comptime(env, inner)?;
                Ok(self.arena.ptr(*is_const, pointee))
            }
            Expr::ArrayType { len, inner, .. } => {
                let el = self.eval_type_comptime(env, inner)?;
                let l = match self.eval_expr(env, len) {
                    Ok(v) => match v.as_int() {
                        // Checked, never truncating: a length above `u64::MAX`
                        // defers rather than wrapping into a smaller array size.
                        Some(n) if n >= 0 => match u64::try_from(n) {
                            Ok(k) => crate::ty::ArrayLen::Known(k),
                            Err(_) => crate::ty::ArrayLen::Deferred,
                        },
                        _ => crate::ty::ArrayLen::Deferred,
                    },
                    Err(Diverge::NotComptime) => crate::ty::ArrayLen::Deferred,
                    Err(other) => return Err(other),
                };
                Ok(self
                    .arena
                    .intern(crate::ty::Type::Array { len: l, elem: el }))
            }
            Expr::Container(c) => Ok(self.eval_container_comptime(env, c)),
            Expr::Comptime { inner, .. } => self.eval_type_comptime(env, inner),
            Expr::Builtin { name, args, span } => {
                // `@Type(info)` / `@typeInfo`-fed type positions.
                match self.eval_builtin_value(env, name, args, *span) {
                    Ok(Value::Type(t)) => Ok(t),
                    Ok(_) => Ok(self.arena.t_deferred()),
                    Err(Diverge::NotComptime) => Ok(self.eval_type(e)),
                    Err(other) => Err(other),
                }
            }
            // Everything else: the static evaluator already handles it.
            _ => Ok(self.eval_type(e)),
        }
    }
}

/// Structural value equality for `switch`/`==` over comptime values, used where
/// the typed `==` path is not taken.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.v == y.v,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Type(x), Value::Type(y)) => x == y,
        (Value::Enum { which: x, .. }, Value::Enum { which: y, .. }) => x == y,
        (Value::Void, Value::Void) => true,
        _ => false,
    }
}

/// The variant name of a bare `.Name` enum/tag literal, if `e` is one.
fn as_tag_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::EnumLiteral { name, .. } => Some(name.clone()),
        _ => None,
    }
}

/// The `f64` view of a numeric value (int or float), for mixed arithmetic.
fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Float { v, .. } => Some(*v),
        _ => None,
    }
}

/// Decodes a `'c'` char literal lexeme into its scalar value.
fn decode_char_literal(text: &str) -> Option<i128> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first == '\\' {
        let esc = chars.next()?;
        let v = match esc {
            'n' => 10,
            'r' => 13,
            't' => 9,
            '\\' => 92,
            '\'' => 39,
            '"' => 34,
            '0' => 0,
            _ => return None,
        };
        return Some(v);
    }
    Some(first as i128)
}

/// Decodes a `"..."` string literal lexeme into its byte content, applying the
/// common escapes. Conservative: an unknown escape is passed through verbatim.
fn decode_str_literal(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('0') => out.push('\0'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
