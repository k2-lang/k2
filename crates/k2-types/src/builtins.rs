//! Compile-time builtins (`@as`, `@sizeOf`, `@TypeOf`, `@typeInfo`, `@import`, …).
//!
//! Reflection / type-producing builtins (`@typeInfo`, `@Type`, `@field`,
//! `@hasField`) are the second genuine comptime boundary: they yield
//! [`Type::Deferred`], but their *arguments* are still synthesized so a concrete
//! error inside an argument is caught. The coercion-checking builtins (`@as`)
//! and the layout builtins (`@sizeOf`/`@alignOf`/`@offsetOf` -> `usize`) are
//! fully modeled.

use k2_syntax::{Expr, Span};

use crate::ty::{IntBits, Type, TypeId};

/// Every builtin name the toolchain implements — the union of those typed here in
/// [`Checker::synth_builtin`] and the "raw"/intrinsic builtins whose result type is
/// supplied by the `check` direction and which the MIR lowerer turns straight into
/// an `Rvalue::Intrinsic`. A `@name` outside this set is a typo or an unimplemented
/// Zig-ism (e.g. `@divTrunc`, which k2 spells `/`); it is reported at the call site
/// instead of silently lowering to `undef` (which prints as `<int>` — a
/// miscompile). Keep in sync when adding a builtin.
pub(crate) const KNOWN_BUILTINS: &[&str] = &[
    "@alignOf",
    "@allocHandle",
    "@allocId",
    "@allocRaw",
    "@arenaDeinit",
    "@as",
    "@atomicCas",
    "@atomicFetchAdd",
    "@atomicLoad",
    "@atomicMake",
    "@atomicStore",
    "@atomicSwap",
    "@bitCast",
    "@bitSizeOf",
    "@bufPrint",
    // The build-graph intrinsics (`std/build.k2`), consumed by the build crate.
    "@build",
    "@buildAddExecutable",
    "@buildAddLibrary",
    "@buildAddRun",
    "@buildAddTest",
    "@buildArtifactForwardArgs",
    "@buildArtifactModule",
    "@buildArtifactModuleSelf",
    "@buildArtifactOption",
    "@buildDependency",
    "@buildDependencyModule",
    "@buildInstall",
    "@buildOption",
    "@buildStdOptimize",
    "@buildStdTarget",
    "@buildStep",
    "@buildStepDependOn",
    "@chanClose",
    "@chanLen",
    "@chanMake",
    "@chanRecv",
    "@chanSend",
    "@clockNow",
    "@clockSleep",
    "@compileError",
    "@compileLog",
    "@createRaw",
    "@destroyRaw",
    "@embedFile",
    "@enumFromInt",
    "@envGet",
    "@errorName",
    "@errorReturnTrace",
    "@field",
    "@FieldType",
    "@floatCast",
    "@floatFromInt",
    "@freeRaw",
    "@fsClose",
    "@fsCreate",
    "@fsDelete",
    "@fsExists",
    "@fsFstat",
    "@fsListDir",
    "@fsMkdir",
    "@fsOpenRead",
    "@fsOpenReadWrite",
    "@fsRead",
    "@fsRmdir",
    "@fsStat",
    "@fsWrite",
    "@fuzzNextU64",
    "@fuzzSeed",
    "@gpaDeinit",
    "@hasDecl",
    "@hasField",
    "@import",
    "@intCast",
    "@intFromEnum",
    "@intFromFloat",
    "@intFromPtr",
    "@max",
    "@min",
    "@mutexLock",
    "@mutexMake",
    "@mutexUnlock",
    "@netAccept",
    "@netClose",
    "@netConnect",
    "@netListen",
    "@netLocalPort",
    "@netRecv",
    "@netSend",
    "@offsetOf",
    "@osArg",
    "@osArgCount",
    "@osArgs",
    "@osExit",
    "@osGetpid",
    "@panic",
    "@ptrCast",
    "@ptrFromInt",
    "@randomBytes",
    "@randomInt",
    "@reduce",
    "@reallocRaw",
    "@schedAwait",
    "@schedRun",
    "@schedSpawn",
    "@schedYield",
    "@sizeOf",
    "@splat",
    "@tagName",
    "@testFail",
    "@testFailEq",
    "@testFailErr",
    "@testFailSlice",
    "@This",
    "@timeMonoReal",
    "@timeSleepReal",
    "@timeWallReal",
    "@truncate",
    "@Type",
    "@typeInfo",
    "@typeName",
    "@TypeOf",
    "@Vector",
    "@wgAdd",
    "@wgDone",
    "@wgMake",
    "@wgWait",
];

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
            // Layout builtins -> usize. The concrete value is computed by the
            // comptime engine and recorded so a `const n = @sizeOf(T)` is itself
            // comptime-known (it drives `[serializedSize(Packet)]u8`).
            "@sizeOf" | "@alignOf" | "@offsetOf" | "@bitSizeOf" => {
                self.synth_all(args);
                let call = Expr::Builtin {
                    name: name.to_string(),
                    args: args.to_vec(),
                    span,
                };
                if let Some(crate::value::Value::Int(ci)) = self.comptime_eval_value(&call) {
                    self.comptime_span_ints.insert((span.start, span.end), ci.v);
                }
                self.arena.t_usize()
            }
            // `@TypeOf(e, ...)` -> the value is a type; record operand types.
            "@TypeOf" => {
                self.synth_all(args);
                self.arena.t_type()
            }
            // `@Vector(N, T)` -> a vector *type* (the value is a `type`). `N` must
            // be comptime-known and `T` numeric (spec §02). A non-comptime length
            // is diagnosed here rather than silently deferring to a `deferred`
            // type the backend cannot lay out.
            "@Vector" => {
                self.synth_all(args);
                if let Some(n_arg) = args.first() {
                    if self
                        .comptime_eval_value(n_arg)
                        .and_then(|v| v.as_int())
                        .is_none()
                    {
                        self.error(span, "`@Vector` length must be comptime-known");
                        return self.arena.t_error();
                    }
                }
                self.arena.t_type()
            }
            // `@splat(value)` -> a vector broadcast. Like `@intCast`, the result
            // type is supplied by context (the expected `@Vector`), so without an
            // expectation it synths to Deferred.
            "@splat" => {
                self.synth_all(args);
                self.arena.t_deferred()
            }
            // `@reduce(.Op, vec)` -> the vector's element type. The second operand
            // must be a `@Vector`; a scalar (or any non-vector) is a diagnostic.
            "@reduce" => {
                if args.len() == 2 {
                    let vt = self.synth(&args[1]);
                    self.synth(&args[0]);
                    if let Type::Vector { elem, .. } = self.arena.get(vt) {
                        return *elem;
                    }
                    if !self.arena.is_bottom(vt) {
                        self.error(
                            span,
                            format!(
                                "`@reduce` expects a `@Vector` operand, found `{}`",
                                self.arena.fmt(vt)
                            ),
                        );
                        return self.arena.t_error();
                    }
                }
                self.synth_all(args);
                self.arena.t_deferred()
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
            // `@errorReturnTrace()` -> an opaque `?*StackTrace` handle. In
            // Debug/ReleaseSafe the runtime yields a non-null handle; in
            // ReleaseFast it yields `null` (the trace machinery is stripped). For
            // v0.20 the value is opaque (the program can null-check / pass it
            // around); the useful product is the automatic trace printed on an
            // error escaping `main`. We model the result as `?<opaque>` so a
            // null-check type-checks.
            "@errorReturnTrace" => {
                self.synth_all(args);
                let inner = self.arena.t_deferred();
                self.arena.optional(inner)
            }
            // String-producing builtins.
            "@errorName" | "@typeName" | "@tagName" | "@embedFile" => {
                self.synth_all(args);
                self.arena.t_str()
            }
            // Diverging builtins. `@compileError`/`@compileLog` fire only when
            // the comptime ENGINE reaches them (an executed branch of a comptime
            // block or an instantiated generic body) — never eagerly here, so a
            // `@compileError` guarded by an `if` that is not taken for a given
            // instantiation does not fire (spec §07.9.1).
            "@compileError" | "@panic" => {
                self.synth_all(args);
                self.arena.t_noreturn()
            }
            "@compileLog" => {
                self.synth_all(args);
                self.arena.t_void()
            }
            // `@This()` -> the enclosing container type (or Deferred at file scope).
            "@This" => self
                .self_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.arena.t_type()),
            // Reflection boundary: try the comptime engine; the result's *type*
            // replaces the v0.5 Deferred when known. Falls back to Deferred when
            // the base is itself comptime-unknown (a module/anytype/std member).
            "@typeInfo" | "@Type" | "@field" | "@hasField" | "@hasDecl" | "@FieldType" => {
                self.synth_all(args);
                let call = Expr::Builtin {
                    name: name.to_string(),
                    args: args.to_vec(),
                    span,
                };
                match self.comptime_eval_value(&call) {
                    Some(v) => self.value_type(&v),
                    None => self.arena.t_deferred(),
                }
            }
            // `@min`/`@max`: every concrete operand must be numeric, and they must
            // mutually unify; the result is that common numeric type. A bottom
            // (Deferred/anytype/error) operand stays conservative. A non-numeric or
            // mutually-incompatible operand is a real error.
            "@min" | "@max" => self.synth_min_max(name, args, span),
            // The std capability/allocator floor builtins (implemented by the VM
            // over the managed heap + *System capabilities). Their result types are
            // fixed so the std source type-checks precisely rather than leaning on
            // Deferred: `@allocId`/`@randomInt`/`@clockNow` yield integers,
            // `@gpaDeinit` a bool, the heap ops a `Deferred` payload (the element
            // type is recovered at run time from the live operand). `@allocHandle`
            // yields the predeclared opaque `Allocator` so a `.allocator()` method
            // returns a value of the same type user signatures spell.
            // The concurrency / scheduler floor (v0.11): the handle-minting
            // builtins yield a `u32` id; `@schedSpawn`/`@chanMake`/`@mutexMake`/
            // `@atomicMake`/`@wgMake` are all id-returning makers.
            "@allocId" | "@schedSpawn" | "@chanMake" | "@mutexMake" | "@atomicMake" | "@wgMake" => {
                self.synth_all(args);
                self.arena.intern(Type::Int {
                    signed: false,
                    bits: IntBits::Fixed(32),
                })
            }
            "@randomInt" | "@clockNow" | "@fuzzNextU64" => {
                self.synth_all(args);
                self.arena.intern(Type::Int {
                    signed: false,
                    bits: IntBits::Fixed(64),
                })
            }
            // `@chanLen` -> `usize`, the buffered count.
            "@chanLen" => {
                self.synth_all(args);
                self.arena.t_usize()
            }
            // `@gpaDeinit` / `@chanSend` -> `bool`.
            "@gpaDeinit" | "@chanSend" => {
                self.synth_all(args);
                self.arena.t_bool()
            }
            // The void-returning scheduler ops. Naming them keeps an expression-
            // statement use (`@mutexLock(id);`) precisely `void` rather than
            // Deferred, so the surrounding `void` method body type-checks exactly.
            "@schedYield" | "@schedRun" | "@chanClose" | "@mutexLock" | "@mutexUnlock"
            | "@atomicStore" | "@wgAdd" | "@wgDone" | "@wgWait"
            // The v0.24 test-runner message recorders + the fuzz seeder are all
            // void-returning effects, so an expression-statement use is exactly
            // `void` (not Deferred) and the surrounding `!void` body type-checks.
            | "@testFail" | "@testFailEq" | "@testFailSlice" | "@testFailErr" | "@fuzzSeed" => {
                self.synth_all(args);
                self.arena.t_void()
            }
            // The optional-returning scheduler ops: `@chanRecv` (`?T`, `null` when
            // closed-and-drained) and `@atomicCas` (`?T`, `null` on success). They
            // MUST synth to a concrete `Optional` so the surrounding `if (x) |v|`
            // unwrap is lowered as an OPTIONAL discriminant, not an error union.
            // The inner type is Deferred (recovered at run time / coerced by the
            // method's `?T` annotation).
            "@chanRecv" | "@atomicCas" => {
                self.synth_all(args);
                let inner = self.arena.t_deferred();
                self.arena.intern(Type::Optional(inner))
            }
            "@allocHandle" => {
                self.synth_all(args);
                self.arena.intern_opaque("Allocator")
            }
            // The remaining floor builtins (`@allocRaw`/`@reallocRaw`/`@freeRaw`/
            // `@createRaw`/`@destroyRaw`/`@arenaDeinit`/`@clockSleep`/
            // `@randomBytes`/`@envGet`/`@bufPrint`) carry their result type from
            // context (the `![]T`/`!*T`/`void`/`?[]const u8` the method annotates),
            // so they synth to Deferred and let the `check` direction supply it.
            // A builtin not typed above: a "raw"/intrinsic builtin whose type the
            // `check` direction supplies (Deferred), OR a genuinely unknown name.
            // Report the latter — a `@divTrunc`/typo would otherwise synth Deferred
            // and lower to a silent `undef` (`<int>`).
            _ => {
                self.synth_all(args);
                if !KNOWN_BUILTINS.contains(&name) {
                    self.error(span, format!("unknown builtin `{name}`"));
                    return self.arena.t_error();
                }
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
