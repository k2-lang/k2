//! Compile-time builtins (`@as`, `@sizeOf`, `@TypeOf`, `@typeInfo`, `@import`, …).
//!
//! Reflection / type-producing builtins (`@typeInfo`, `@Type`, `@field`,
//! `@hasField`) are the second genuine comptime boundary: they yield
//! [`Type::Deferred`], but their *arguments* are still synthesized so a concrete
//! error inside an argument is caught. The coercion-checking builtins (`@as`)
//! and the layout builtins (`@sizeOf`/`@alignOf`/`@offsetOf` -> `usize`) are
//! fully modeled.

use k2_syntax::{Expr, Span};

use crate::ty::TypeId;

impl crate::check::Checker<'_> {
    /// Synthesizes a builtin call by name.
    pub(crate) fn synth_builtin(&mut self, name: &str, args: &[Expr], span: Span) -> TypeId {
        match name {
            // `@as(T, e)`: the explicit *widening* coercion (spec §12.2). Check
            // `e` is coercible to `T` under the widening relation (which also
            // accepts same-signedness integer widening, e.g. `u8 -> u32`).
            "@as" => {
                if args.len() == 2 {
                    let t = self.eval_type(&args[0]);
                    // A compile-time-known integer literal (or a negated one) is
                    // range-checked against the target exactly like the direct
                    // `const x: T = <lit>` path, so `@as(u8, 300)` / `@as(u8, -1)` /
                    // `@as(i8, 200)` error at the coercion site (spec §02 says a
                    // compile-time coercion that does not fit is an error here).
                    // Non-literal operands keep the widening `as_coerces` relation.
                    if self.is_int_literal_expr(&args[1]) {
                        self.check_int_value_against(&args[1], t);
                        self.record(args[1].span(), t);
                        return t;
                    }
                    let got = self.synth(&args[1]);
                    if !self.arena.as_coerces(got, t) {
                        self.error(
                            args[1].span(),
                            format!(
                                "`@as` cannot coerce `{}` to `{}`",
                                self.arena.fmt(got),
                                self.arena.fmt(t)
                            ),
                        );
                    }
                    self.record(args[1].span(), t);
                    t
                } else {
                    self.synth_all(args);
                    self.arena.t_deferred()
                }
            }
            // Layout builtins -> usize.
            "@sizeOf" | "@alignOf" | "@offsetOf" | "@bitSizeOf" => {
                self.synth_all(args);
                self.arena.t_usize()
            }
            // `@TypeOf(e, ...)` -> the value is a type; record operand types.
            "@TypeOf" => {
                self.synth_all(args);
                self.arena.t_type()
            }
            // Casts: permissive; their result type is whatever the context wants,
            // so without an expectation they synth to Deferred (the `check`
            // direction supplies the target).
            "@intCast" | "@ptrCast" | "@truncate" | "@bitCast" | "@floatCast" | "@enumFromInt"
            | "@intFromEnum" | "@intFromFloat" | "@floatFromInt" | "@ptrFromInt"
            | "@intFromPtr" => {
                self.synth_all(args);
                self.arena.t_deferred()
            }
            // `@import("s")` -> a module namespace (or Deferred if unmapped).
            "@import" => self.import_namespace(args, span),
            // String-producing builtins.
            "@errorName" | "@typeName" | "@tagName" | "@embedFile" => {
                self.synth_all(args);
                self.arena.t_str()
            }
            // Diverging builtins.
            "@compileError" | "@panic" => {
                self.synth_all(args);
                self.arena.t_noreturn()
            }
            // `@This()` -> the enclosing container type (or Deferred at file scope).
            "@This" => self
                .self_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.arena.t_type()),
            // Reflection boundary -> Deferred; still synth args for inner errors.
            "@typeInfo" | "@Type" | "@field" | "@hasField" | "@hasDecl" | "@FieldType" => {
                self.synth_all(args);
                self.arena.t_deferred()
            }
            // `@min`/`@max`: every concrete operand must be numeric, and they must
            // mutually unify; the result is that common numeric type. A bottom
            // (Deferred/anytype/error) operand stays conservative. A non-numeric or
            // mutually-incompatible operand is a real error.
            "@min" | "@max" => self.synth_min_max(name, args, span),
            // Unknown builtin: conservative Deferred, still synth args.
            _ => {
                self.synth_all(args);
                self.arena.t_deferred()
            }
        }
    }

    /// Synthesizes `@min(a, b, ...)` / `@max(...)`: requires every concrete
    /// operand to be numeric and to mutually unify (folding `comptime_int` to its
    /// sized peer), returning the common numeric type. A bottom operand keeps the
    /// result conservative (Deferred); a non-numeric or incompatible operand is a
    /// reported error whose result is `<error>`.
    fn synth_min_max(&mut self, name: &str, args: &[Expr], span: Span) -> TypeId {
        let mut acc: Option<TypeId> = None;
        let mut any_bottom = false;
        let mut failed = false;
        for a in args {
            let t = self.synth(a);
            if self.arena.is_bottom(t) {
                any_bottom = true;
                continue;
            }
            if !self.numeric(t) {
                self.error(
                    a.span(),
                    format!(
                        "`{}` requires numeric operands, found `{}`",
                        name,
                        self.arena.fmt(t)
                    ),
                );
                failed = true;
                continue;
            }
            acc = Some(match acc {
                None => t,
                Some(prev) => match self.try_unify_numeric(prev, t) {
                    Some(common) => common,
                    None => {
                        self.error(
                            span,
                            format!(
                                "`{}` operands have incompatible types `{}` and `{}`",
                                name,
                                self.arena.fmt(prev),
                                self.arena.fmt(t)
                            ),
                        );
                        failed = true;
                        prev
                    }
                },
            });
        }
        if failed {
            return self.arena.t_error();
        }
        // If any operand was bottom, the result is comptime-unknown.
        if any_bottom {
            return self.arena.t_deferred();
        }
        acc.unwrap_or_else(|| self.arena.t_deferred())
    }

    /// Synthesizes each argument (for effect). Type-name arguments (`u32`,
    /// `Packet`, `T`) synth cleanly to `type`; value arguments synth to their
    /// binding type — so a single value-synth is safe for every builtin argument.
    fn synth_all(&mut self, args: &[Expr]) {
        for a in args {
            self.synth(a);
        }
    }

    /// Resolves `@import("s")` to a module namespace type from the resolver's
    /// module graph, or Deferred if it cannot be mapped.
    fn import_namespace(&mut self, args: &[Expr], span: Span) -> TypeId {
        // The resolver bound the surrounding `const X = @import(...)` to a module
        // def; for a bare `@import` used inline we look up the module node by the
        // call span if recorded, else Deferred.
        let _ = args;
        let _ = span;
        // Member access on a module is Deferred anyway, so returning a module or
        // Deferred is equivalent for checking; keep it simple and Deferred unless
        // a const binding already typed it.
        self.arena.t_deferred()
    }
}
