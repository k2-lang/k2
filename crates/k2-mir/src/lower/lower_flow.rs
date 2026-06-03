//! Control-flow lowering: `if`/`while`/`for`/`switch`, labeled blocks,
//! `break`/`continue` (labeled + break-with-value).
//!
//! Every form here desugars into basic blocks and explicit terminators — there
//! is no hidden control flow in the resulting MIR. When a form is used as an
//! expression, the caller supplies a destination [`Place`]; the arms write that
//! slot and join. `for` desugars to an index-driven `while`; the lockstep-length
//! safety check for multi-operand `for` is inserted in safe builds.

use super::*;

impl FnBuilder<'_, '_> {
    // ===================================================================
    //  if
    // ===================================================================

    /// Lowers an `if` (statement or expression). `dst` is `Some` in expression
    /// position; both arms write it and join.
    pub(super) fn lower_if(&mut self, e: &Expr, dst: Option<Place>) {
        let Expr::If {
            cond,
            capture,
            then_branch,
            else_branch,
            span,
            ..
        } = e
        else {
            return;
        };
        // Optional/error-union capture form: branch on the discriminant.
        if capture.is_some() {
            self.lower_if_capture(e, dst);
            return;
        }
        let cond_op = self.lower_operand(cond, self.lo.typed.arena.t_bool());
        let then_bb = self.new_block();
        let else_bb = self.new_block();
        let join = self.new_block();
        self.set_term(Terminator::Branch {
            cond: cond_op,
            then_bb,
            else_bb,
        });
        // then arm.
        self.cur = then_bb;
        self.lower_branch_value(then_branch, dst.clone());
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        // else arm.
        self.cur = else_bb;
        if let Some(eb) = else_branch {
            self.lower_branch_value(eb, dst);
        } else if let Some(d) = dst {
            // No else in expr position: void result.
            self.assign(d, Rvalue::Use(Operand::Const(Const::Void)), *span);
        }
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        self.cur = join;
    }

    /// Lowers an `if (payload) |cap| ... else |e| ...` capture form (the branch
    /// taken when the `if` has a payload capture). `e` is the [`Expr::If`].
    fn lower_if_capture(&mut self, e: &Expr, dst: Option<Place>) {
        let Expr::If {
            cond,
            capture,
            then_branch,
            else_capture,
            else_branch,
            span,
        } = e
        else {
            return;
        };
        let span = *span;
        let capture = capture.as_ref().expect("capture form");
        let then_branch = then_branch.as_ref();
        let else_capture = else_capture.as_ref();
        let else_branch = else_branch.as_deref();
        let cond_ty = self.type_at(cond.span());
        let is_optional = matches!(self.lo.typed.arena.get(cond_ty), Type::Optional(_));
        let kind = if is_optional {
            DiscrKind::Optional
        } else {
            DiscrKind::ErrorUnion
        };
        let cond_op = self.lower_operand(cond, cond_ty);
        let discr = self.new_temp(self.lo.typed.arena.t_bool(), span);
        self.assign(
            Place::local(discr),
            Rvalue::Discriminant {
                operand: cond_op.clone(),
                kind,
            },
            span,
        );
        // For an optional: discr == is_null. For an error union: discr == is_err.
        // The "payload present" branch is the negative one.
        let payload_bb = self.new_block();
        let empty_bb = self.new_block();
        let join = self.new_block();
        self.set_term(Terminator::Branch {
            cond: Operand::local(discr),
            then_bb: empty_bb,
            else_bb: payload_bb,
        });
        // payload branch: bind |cap| to the payload, lower then.
        self.cur = payload_bb;
        let payload_ty = if is_optional {
            self.optional_inner_pub(cond_ty)
        } else {
            self.error_union_ok(cond_ty)
        };
        let payload = self.operand_payload(cond_op.clone(), payload_ty, kind);
        self.bind_capture(capture, payload, payload_ty, span);
        self.lower_branch_value(then_branch, dst.clone());
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        // empty branch: bind else |e| if present (the error value), lower else.
        self.cur = empty_bb;
        if let Some(ec) = else_capture {
            let err_ty = cond_ty;
            self.bind_capture(ec, cond_op, err_ty, span);
        }
        if let Some(eb) = else_branch {
            self.lower_branch_value(eb, dst);
        } else if let Some(d) = dst {
            self.assign(d, Rvalue::Use(Operand::Const(Const::Void)), span);
        }
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        self.cur = join;
    }

    /// Lowers a branch body that may be a block or a single value expression,
    /// writing its value into `dst` when present.
    fn lower_branch_value(&mut self, body: &Expr, dst: Option<Place>) {
        match (body, dst) {
            (Expr::Block { label, body, .. }, dst) => {
                self.lower_block_expr(label.as_deref(), body, dst);
            }
            (e, Some(d)) => self.lower_into(d, e),
            (e, None) => self.lower_for_effect(e),
        }
    }

    // ===================================================================
    //  while
    // ===================================================================

    /// Lowers a `while (cond) [: (cont)] body [else e]`.
    pub(super) fn lower_while(&mut self, e: &Expr, dst: Option<Place>) {
        let Expr::While {
            label,
            cond,
            capture,
            cont,
            body,
            else_branch,
            span,
            ..
        } = e
        else {
            return;
        };
        self.alloc_info.has_loop = true;
        let header = self.new_block();
        let body_bb = self.new_block();
        let cont_bb = self.new_block();
        let done = self.new_block();
        self.set_term(Terminator::Goto(header));
        // header: test cond.
        self.cur = header;
        let cond_ty = self.type_at(cond.span());
        if capture.is_some() {
            // while (opt) |x| : branch on discriminant.
            let is_optional = matches!(self.lo.typed.arena.get(cond_ty), Type::Optional(_));
            let kind = if is_optional {
                DiscrKind::Optional
            } else {
                DiscrKind::ErrorUnion
            };
            let cond_op = self.lower_operand(cond, cond_ty);
            let discr = self.new_temp(self.lo.typed.arena.t_bool(), *span);
            self.assign(
                Place::local(discr),
                Rvalue::Discriminant {
                    operand: cond_op.clone(),
                    kind,
                },
                *span,
            );
            self.set_term(Terminator::Branch {
                cond: Operand::local(discr),
                then_bb: done,
                else_bb: body_bb,
            });
            self.cur = body_bb;
            let payload_ty = if is_optional {
                self.optional_inner_pub(cond_ty)
            } else {
                self.error_union_ok(cond_ty)
            };
            let payload = self.operand_payload(cond_op, payload_ty, kind);
            self.bind_capture(capture.as_ref().unwrap(), payload, payload_ty, *span);
        } else {
            let cond_op = self.lower_operand(cond, self.lo.typed.arena.t_bool());
            self.set_term(Terminator::Branch {
                cond: cond_op,
                then_bb: body_bb,
                else_bb: done,
            });
            self.cur = body_bb;
        }
        // Loop context for break/continue.
        self.push_scope(label.clone());
        self.scopes.last_mut().unwrap().loop_ctx = Some(LoopCtx {
            continue_bb: cont_bb,
            break_bb: done,
            break_val: dst.clone().map(|p| p.base),
        });
        self.lower_loop_body(body);
        if !self.terminated() {
            self.set_term(Terminator::Goto(cont_bb));
        }
        self.pop_scope_no_defers();
        // cont: run the continue-expr (if any), then back to header.
        self.cur = cont_bb;
        if let Some(c) = cont {
            self.lower_stmt(c);
        }
        if !self.terminated() {
            self.set_term(Terminator::Goto(header));
        }
        // done: the else clause (no-break value) in expr position.
        self.cur = done;
        if let Some(eb) = else_branch {
            self.lower_branch_value(eb, dst);
        } else if let Some(d) = dst {
            self.assign(d, Rvalue::Use(Operand::Const(Const::Void)), *span);
        }
    }

    // ===================================================================
    //  for
    // ===================================================================

    /// Lowers a `for (operands) |captures| body [else e]` into an index-driven
    /// loop. Handles slice/array operands (value + by-pointer captures), bounded
    /// ranges, the running-index `0..`, and lockstep multi-operand iteration.
    pub(super) fn lower_for(&mut self, e: &Expr, dst: Option<Place>) {
        let Expr::For {
            label,
            operands,
            captures,
            body,
            else_branch,
            span,
            ..
        } = e
        else {
            return;
        };
        self.alloc_info.has_loop = true;
        let usize_ty = self.lo.typed.arena.t_usize();
        // Classify operands: collection places (with their lengths) and ranges.
        let mut coll_places: Vec<(Place, Operand, Option<Expr>)> = Vec::new();
        let mut range_lo: Option<Operand> = None;
        let mut range_hi: Option<Operand> = None;
        let mut open_index_start: Option<Operand> = None;
        for op in operands {
            match op {
                ForOperand::Value(v) => {
                    let place = self.lower_place_autoderef_pub(v);
                    let len = self.slice_len_operand(&place, v);
                    coll_places.push((place, len, Some(v.clone())));
                }
                ForOperand::Range { lo, hi, .. } => {
                    let lo_op = self.lower_operand(lo, usize_ty);
                    match hi {
                        Some(h) => {
                            let hi_op = self.lower_operand(h, usize_ty);
                            range_lo = Some(lo_op);
                            range_hi = Some(hi_op);
                        }
                        None => {
                            // `0..` — a running index alongside collections.
                            open_index_start = Some(lo_op);
                        }
                    }
                }
            }
        }
        // The trip count: a bounded range's (hi-lo), else the first collection's
        // length. In safe builds, multiple collections get a LenEq check.
        let count = self.for_trip_count(&coll_places, &range_lo, &range_hi, *span);
        // Lockstep length checks for multiple collection operands.
        if self.lo.mode.checks_enabled() && coll_places.len() >= 2 {
            for w in coll_places.windows(2) {
                self.emit(Statement::Check(SafetyCheck {
                    kind: CheckKind::LenEq {
                        a: w[0].1.clone(),
                        b: w[1].1.clone(),
                    },
                    span: *span,
                }));
            }
        }
        // Index local `i`, initialized to 0 (or the range's lo).
        let i = self.new_temp(usize_ty, *span);
        let init = range_lo.clone().unwrap_or(Operand::Const(Const::Int {
            value: 0,
            ty: usize_ty,
        }));
        self.assign(Place::local(i), Rvalue::Use(init), *span);
        let header = self.new_block();
        let body_bb = self.new_block();
        let cont_bb = self.new_block();
        let done = self.new_block();
        self.set_term(Terminator::Goto(header));
        // header: i < count.
        self.cur = header;
        let cond = self.new_temp(self.lo.typed.arena.t_bool(), *span);
        self.assign(
            Place::local(cond),
            Rvalue::Binary {
                op: BinOp::Lt,
                lhs: Operand::local(i),
                rhs: count,
                ty: self.lo.typed.arena.t_bool(),
            },
            *span,
        );
        self.set_term(Terminator::Branch {
            cond: Operand::local(cond),
            then_bb: body_bb,
            else_bb: done,
        });
        // body: bind captures, then lower the body.
        self.cur = body_bb;
        self.bind_for_captures(
            captures,
            &coll_places,
            i,
            &range_lo,
            &open_index_start,
            *span,
        );
        self.push_scope(label.clone());
        self.scopes.last_mut().unwrap().loop_ctx = Some(LoopCtx {
            continue_bb: cont_bb,
            break_bb: done,
            break_val: dst.clone().map(|p| p.base),
        });
        self.lower_loop_body(body);
        if !self.terminated() {
            self.set_term(Terminator::Goto(cont_bb));
        }
        self.pop_scope_no_defers();
        // cont: i += 1; back to header.
        self.cur = cont_bb;
        self.assign(
            Place::local(i),
            Rvalue::Binary {
                op: BinOp::Add,
                lhs: Operand::local(i),
                rhs: Operand::Const(Const::Int {
                    value: 1,
                    ty: usize_ty,
                }),
                ty: usize_ty,
            },
            *span,
        );
        self.set_term(Terminator::Goto(header));
        // done: else clause (no-break value).
        self.cur = done;
        if let Some(eb) = else_branch {
            self.lower_branch_value(eb, dst);
        } else if let Some(d) = dst {
            self.assign(d, Rvalue::Use(Operand::Const(Const::Void)), *span);
        }
    }

    /// The loop *bound* operand for a `for` loop's `i < bound` header.
    ///
    /// The index local `i` is initialized to the range's `lo` (or `0`) and stepped
    /// by `1` each iteration (see [`Self::lower_for`]). The header therefore tests
    /// `i < bound` where `bound` is the *exclusive upper limit* — so for a bounded
    /// range `lo..hi` the bound is `hi` directly (NOT `hi - lo`): `for (2..5)`
    /// starts `i = 2` and runs while `i < 5`, i.e. `i = 2, 3, 4` (three iterations).
    /// A zero-start range `0..n` reduces to the obvious `i < n`. With no range the
    /// bound is the first collection's length (`i < coll.len`), else `0`.
    fn for_trip_count(
        &mut self,
        coll: &[(Place, Operand, Option<Expr>)],
        range_lo: &Option<Operand>,
        range_hi: &Option<Operand>,
        _span: Span,
    ) -> Operand {
        let usize_ty = self.lo.typed.arena.t_usize();
        let _ = range_lo;
        if let Some(hi) = range_hi {
            // `i` already starts at `lo`; the exclusive bound is `hi` itself.
            return hi.clone();
        }
        if let Some((_, len, _)) = coll.first() {
            return len.clone();
        }
        Operand::Const(Const::Int {
            value: 0,
            ty: usize_ty,
        })
    }

    /// Binds the `for` captures for one iteration: each collection capture reads
    /// `coll[i]` (by value or by `&coll[i]`), and an index capture reads `i`.
    fn bind_for_captures(
        &mut self,
        captures: &[CaptureName],
        coll: &[(Place, Operand, Option<Expr>)],
        i: LocalId,
        range_lo: &Option<Operand>,
        open_index_start: &Option<Operand>,
        span: Span,
    ) {
        let usize_ty = self.lo.typed.arena.t_usize();
        let mut coll_idx = 0;
        for cap in captures {
            if cap.name == "_" {
                // A discard for a collection slot still advances the operand index.
                if coll_idx < coll.len() {
                    coll_idx += 1;
                }
                continue;
            }
            let def = self.capture_name_def(cap);
            // Is this capture bound to a collection operand or to the index?
            if coll_idx < coll.len() {
                let (place, _, base_expr) = &coll[coll_idx];
                coll_idx += 1;
                let elem_ty = self.element_type_of(base_expr.as_ref());
                let elem_place = place.project(Proj::Index {
                    index: Operand::local(i),
                    ty: elem_ty,
                });
                if cap.by_ref {
                    // |*slot| : a pointer to the element.
                    self.mark_address_taken(place.base);
                    let ptr_ty = self.elem_ptr_type(elem_ty, base_expr.as_ref());
                    let local = self.declare_local(def, ptr_ty, span);
                    self.assign(
                        Place::local(local),
                        Rvalue::Ref {
                            place: elem_place,
                            is_const: matches!(
                                self.lo.typed.arena.get(ptr_ty),
                                Type::Pointer { is_const: true, .. }
                            ),
                            ty: ptr_ty,
                        },
                        span,
                    );
                } else {
                    // |item| : a copy of the element.
                    let local = self.declare_local(def, elem_ty, span);
                    self.assign(
                        Place::local(local),
                        Rvalue::Use(Operand::Copy(elem_place)),
                        span,
                    );
                }
            } else {
                // The running index: i (offset by the open-range start if any).
                let local = self.declare_local(def, usize_ty, span);
                let idx_val =
                    if let Some(start) = open_index_start.clone().or_else(|| range_lo.clone()) {
                        // idx = start + i (start already folded into i for ranges, so
                        // for an open `0..` we add the explicit start).
                        if open_index_start.is_some() {
                            let t = self.new_temp(usize_ty, span);
                            self.assign(
                                Place::local(t),
                                Rvalue::Binary {
                                    op: BinOp::Add,
                                    lhs: start,
                                    rhs: Operand::local(i),
                                    ty: usize_ty,
                                },
                                span,
                            );
                            Operand::local(t)
                        } else {
                            Operand::local(i)
                        }
                    } else {
                        Operand::local(i)
                    };
                self.assign(Place::local(local), Rvalue::Use(idx_val), span);
            }
        }
    }

    /// The element type a collection operand iterates.
    fn element_type_of(&self, base: Option<&Expr>) -> TypeId {
        let Some(b) = base else {
            return self.lo.typed.arena.t_deferred();
        };
        let bty = self.type_at(b.span());
        match self.lo.typed.arena.get(self.peel_ptr_pub(bty)) {
            Type::Slice { elem, .. } | Type::Array { elem, .. } => *elem,
            _ => self.lo.typed.arena.t_deferred(),
        }
    }

    /// The `*elem` pointer type for a by-ref `for` capture.
    fn elem_ptr_type(&mut self, elem_ty: TypeId, base: Option<&Expr>) -> TypeId {
        let is_const = base
            .map(|b| {
                let bty = self.type_at(b.span());
                matches!(
                    self.lo.typed.arena.get(self.peel_ptr_pub(bty)),
                    Type::Slice { is_const: true, .. }
                )
            })
            .unwrap_or(false);
        self.lo.typed.arena.ptr(is_const, elem_ty)
    }

    // ===================================================================
    //  switch
    // ===================================================================

    /// Lowers a `switch (scrutinee) { arms }` into a `Switch` terminator (for
    /// integer/enum/error scrutinees) or a guard-chain (for ranges).
    pub(super) fn lower_switch(&mut self, e: &Expr, dst: Option<Place>) {
        let Expr::Switch {
            scrutinee,
            arms,
            span,
        } = e
        else {
            return;
        };
        let scrut_ty = self.type_at(scrutinee.span());
        let scrut = self.lower_operand(scrutinee, scrut_ty);
        // Lower the scrutinee to an integer discriminant for the Switch.
        let int_scrut = self.switch_scrutinee_int(scrut.clone(), scrut_ty, *span);
        let join = self.new_block();
        // Single-value items become discrete `Switch` targets. Inclusive ranges
        // (`lo...hi`) become a *guard chain* (`lo <= scrut && scrut <= hi`) tested
        // BEFORE the Switch — we must NOT enumerate range values, or a range like
        // `0...u64::MAX` would materialize ~2^64 targets and hang the lowerer.
        let mut targets: Vec<(i128, BlockId)> = Vec::new();
        let mut ranges: Vec<(i128, i128, BlockId)> = Vec::new();
        let mut default_bb: Option<BlockId> = None;
        // Pre-allocate an arm block per arm so we can reference them in the Switch.
        let mut arm_blocks: Vec<BlockId> = Vec::with_capacity(arms.len());
        for _ in arms {
            arm_blocks.push(self.new_block());
        }
        for (ai, arm) in arms.iter().enumerate() {
            let arm_bb = arm_blocks[ai];
            match &arm.pattern {
                SwitchPattern::Else => {
                    default_bb = Some(arm_bb);
                }
                SwitchPattern::Items(items) => {
                    for it in items {
                        if let Some(v) = self.switch_item_value(&it.lo, scrut_ty) {
                            match &it.hi {
                                None => targets.push((v, arm_bb)),
                                Some(hi) => {
                                    // Inclusive range lo...hi: a guard, not values.
                                    // A degenerate single-point range folds to one
                                    // discrete target.
                                    if let Some(hv) = self.switch_item_value(hi, scrut_ty) {
                                        if v == hv {
                                            targets.push((v, arm_bb));
                                        } else if v < hv {
                                            ranges.push((v, hv, arm_bb));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // A default block: the explicit `else` arm, or a trap (exhaustive switch).
        let default = match default_bb {
            Some(b) => b,
            None => {
                let trap = self.new_block();
                self.func.blocks[trap.index()].is_panic = true;
                self.func.blocks[trap.index()].term = Terminator::Trap {
                    reason: TrapReason::Unreachable,
                };
                trap
            }
        };
        // Emit the range guard chain in the current block, then the Switch in the
        // final fall-through block. Each guard tests `lo <= scrut && scrut <= hi`
        // and branches to the arm; a miss flows to the next guard / the Switch.
        let switch_bb = self.lower_switch_range_guards(&int_scrut, scrut_ty, &ranges, *span);
        self.cur = switch_bb;
        self.set_term(Terminator::Switch {
            scrutinee: int_scrut,
            targets,
            default,
        });
        // Lower each arm body.
        for (ai, arm) in arms.iter().enumerate() {
            let arm_bb = arm_blocks[ai];
            self.cur = arm_bb;
            // Bind a payload capture (tagged-union arm) if present.
            if let Some(cap) = &arm.capture {
                // The payload type is the scrutinee for an error/enum; for a
                // tagged union it is the variant payload. Use the arm type.
                let payload_ty = self.type_at(arm.body.span());
                self.bind_capture(cap, scrut.clone(), payload_ty, *span);
            }
            self.lower_branch_value(&arm.body, dst.clone());
            if !self.terminated() {
                self.set_term(Terminator::Goto(join));
            }
        }
        self.cur = join;
    }

    /// Emits a guard chain for a switch's inclusive-range arms, starting in
    /// `self.cur`, and returns the block where the residual `Switch` terminator
    /// should be placed.
    ///
    /// Each range `(lo, hi, arm)` becomes `lo <= scrut && scrut <= hi -> arm`,
    /// else fall through to the next guard (and finally to the returned block, on
    /// which the caller installs the `Switch` over the single-value targets). This
    /// keeps lowering time/memory independent of the range *magnitude* — a single
    /// `0...u64::MAX` arm costs one guard block, not 2^64 Switch targets.
    fn lower_switch_range_guards(
        &mut self,
        scrut: &Operand,
        scrut_ty: TypeId,
        ranges: &[(i128, i128, BlockId)],
        span: Span,
    ) -> BlockId {
        // The comparison constants take the scrutinee's integer type (ranges only
        // apply to integer/char scrutinees), defaulting to `usize` otherwise.
        let cmp_ty = match self.lo.typed.arena.get(scrut_ty) {
            Type::Int { .. } | Type::ComptimeInt => scrut_ty,
            _ => self.lo.typed.arena.t_usize(),
        };
        let bool_ty = self.lo.typed.arena.t_bool();
        for &(lo, hi, arm_bb) in ranges {
            // `lo <= scrut`?  -> if false, skip to the next guard.
            let ge_lo = self.new_temp(bool_ty, span);
            self.assign(
                Place::local(ge_lo),
                Rvalue::Binary {
                    op: BinOp::Ge,
                    lhs: scrut.clone(),
                    rhs: Operand::Const(Const::Int {
                        value: lo,
                        ty: cmp_ty,
                    }),
                    ty: bool_ty,
                },
                span,
            );
            let check_hi = self.new_block();
            let next = self.new_block();
            self.set_term(Terminator::Branch {
                cond: Operand::local(ge_lo),
                then_bb: check_hi,
                else_bb: next,
            });
            // `scrut <= hi`? -> if true, take the arm; else the next guard.
            self.cur = check_hi;
            let le_hi = self.new_temp(bool_ty, span);
            self.assign(
                Place::local(le_hi),
                Rvalue::Binary {
                    op: BinOp::Le,
                    lhs: scrut.clone(),
                    rhs: Operand::Const(Const::Int {
                        value: hi,
                        ty: cmp_ty,
                    }),
                    ty: bool_ty,
                },
                span,
            );
            self.set_term(Terminator::Branch {
                cond: Operand::local(le_hi),
                then_bb: arm_bb,
                else_bb: next,
            });
            self.cur = next;
        }
        // The residual block (after all guards missed) hosts the value Switch.
        self.cur
    }

    /// Lowers a switch scrutinee to an integer discriminant suitable for a
    /// `Switch` terminator. For an enum/error scrutinee we read its discriminant;
    /// for an integer it is used directly.
    fn switch_scrutinee_int(&mut self, scrut: Operand, scrut_ty: TypeId, span: Span) -> Operand {
        match self.lo.typed.arena.get(scrut_ty) {
            Type::Enum(_) | Type::Union(_) => {
                let t = self.new_temp(self.lo.typed.arena.t_usize(), span);
                self.assign(
                    Place::local(t),
                    Rvalue::Discriminant {
                        operand: scrut,
                        kind: DiscrKind::Union,
                    },
                    span,
                );
                Operand::local(t)
            }
            // Error sets compare by tag; map to the tag integer.
            Type::ErrorSet(_) | Type::AnyError => scrut,
            _ => scrut,
        }
    }

    /// The integer value of a switch item pattern (an int literal, char, or
    /// `error.X`/`.Variant` whose tag/index is known).
    fn switch_item_value(&mut self, e: &Expr, scrut_ty: TypeId) -> Option<i128> {
        match e {
            Expr::Int { text, base, .. } => super::lower_expr::parse_int(text, *base),
            Expr::Char { text, .. } => super::lower_expr::parse_char(text).map(|v| v as i128),
            Expr::ErrorLiteral { name, .. } => Some(self.lo.err_tag(name).0 as i128),
            Expr::Field { base, field, span } => {
                // `Set.Member` or `Enum.Variant`.
                if self.error_set_member(base, field).is_some() {
                    return Some(self.lo.err_tag(field).0 as i128);
                }
                match self.member_at(*span) {
                    Some(MemberRes::Variant(idx)) => Some(idx as i128),
                    Some(MemberRes::ErrorMember) => Some(self.lo.err_tag(field).0 as i128),
                    _ => None,
                }
            }
            Expr::EnumLiteral { span, .. } => match self.member_at(*span) {
                Some(MemberRes::Variant(idx)) => Some(idx as i128),
                _ => None,
            },
            _ => {
                let _ = scrut_ty;
                None
            }
        }
    }

    // ===================================================================
    //  blocks / break / continue
    // ===================================================================

    /// Lowers a labeled or bare block-as-expression, supporting `break :label v`.
    pub(super) fn lower_block_expr(
        &mut self,
        label: Option<&str>,
        body: &[Stmt],
        dst: Option<Place>,
    ) {
        let join = self.new_block();
        self.push_scope(label.map(|s| s.to_string()));
        self.scopes.last_mut().unwrap().block_break = Some((dst.clone().map(|p| p.base), join));
        self.lower_block_stmts(body);
        if !self.terminated() {
            self.pop_scope_run_defers_into(join);
        } else {
            self.pop_scope_no_defers();
        }
        self.cur = join;
        let _ = dst;
    }

    /// Runs the innermost scope's defers, jumps to `join`, and pops the frame.
    fn pop_scope_run_defers_into(&mut self, join: BlockId) {
        let idx = self.scopes.len() - 1;
        self.run_defers_in_frame(idx, false);
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        self.scopes.pop();
    }

    /// Lowers a loop body (a block or single expression) in its OWN lexical scope
    /// frame, nested *inside* the loop-control frame, and runs that body frame's
    /// `defer`/`errdefer` actions on the normal per-iteration fall-through.
    ///
    /// Because the body frame sits above the loop-control frame on the scope stack,
    /// `break`/`continue` — which run [`Self::run_defers_until`] down to (and
    /// excluding) the loop frame — also run these body defers exactly once on the
    /// way out. The loop-control frame itself carries no defers (it only holds the
    /// `loop_ctx`), so a body defer runs once per iteration exit and never twice.
    fn lower_loop_body(&mut self, body: &Expr) {
        self.push_scope(None);
        match body {
            Expr::Block { body, .. } => self.lower_block_stmts(body),
            e => self.lower_for_effect(e),
        }
        // Fall-through end of the iteration: run the body's defers, then pop. A
        // `break`/`continue` already terminated the block and ran these defers via
        // `run_defers_until`, so we only run them here when control falls through.
        if !self.terminated() {
            self.pop_scope_run_defers();
        } else {
            self.pop_scope_no_defers();
        }
    }

    /// Lowers a `break [:label] [value]`.
    ///
    /// A *bare* (unlabeled) `break` may only target the innermost LOOP — never an
    /// enclosing unlabeled block-expr (in particular the synthetic `Expr::Block`
    /// the parser wraps a statement-form `if`/`while`/`for` body in). We therefore
    /// pass `allow_block = label.is_some()`: when a label is present the `Some(l)`
    /// arm of [`Self::find_target_scope`] matches the labeled block-expr (or loop)
    /// independently of `allow_block`; when it is absent we skip every block frame
    /// and bind to the nearest `loop_ctx`. Matching a bare break against a block
    /// frame would let `if (c) break;` jump to the if-wrapper's join and fall back
    /// into the loop body instead of leaving the loop (spec: breaking a *block*
    /// requires a label).
    pub(super) fn lower_break(&mut self, label: Option<&str>, value: Option<&Expr>) {
        let Some(target_idx) = self.find_target_scope(label, label.is_some()) else {
            return;
        };
        // Write the break value into the target's break-value slot.
        let (break_bb, val_slot) = self.break_target(target_idx);
        if let (Some(v), Some(slot)) = (value, val_slot) {
            // The block-result slot is declared from the *binding's* type, which
            // the checker types `noreturn` when the block's last statement is a
            // diverging `break :blk` (see the type-check of a block whose value
            // comes only from breaks). Re-type the slot from the actual value's
            // type so the result is `u32` (etc.), not the bottom `noreturn`.
            let slot_ty = self.func.locals[slot.index()].ty;
            if matches!(
                self.lo.typed.arena.get(slot_ty),
                Type::NoReturn | Type::Deferred
            ) {
                let vty = self.type_at(v.span());
                if !matches!(self.lo.typed.arena.get(vty), Type::NoReturn) {
                    self.func.locals[slot.index()].ty = vty;
                }
            }
            self.lower_into(Place::local(slot), v);
        }
        // Run defers between here and the target (exclusive).
        self.run_defers_until(target_idx);
        self.set_term(Terminator::Goto(break_bb));
    }

    /// Lowers a `continue [:label]`.
    pub(super) fn lower_continue(&mut self, label: Option<&str>) {
        let Some(target_idx) = self.find_target_scope(label, false) else {
            return;
        };
        let cont_bb = self.scopes[target_idx]
            .loop_ctx
            .as_ref()
            .map(|c| c.continue_bb);
        if let Some(bb) = cont_bb {
            self.run_defers_until(target_idx);
            self.set_term(Terminator::Goto(bb));
        }
    }

    /// Finds the index of the scope frame a `break`/`continue` targets. With a
    /// label: the nearest frame with that label. Without: the nearest loop (for
    /// continue) or the nearest loop/labeled-block (for break).
    fn find_target_scope(&self, label: Option<&str>, allow_block: bool) -> Option<usize> {
        for idx in (0..self.scopes.len()).rev() {
            let f = &self.scopes[idx];
            match label {
                Some(l) => {
                    if f.label.as_deref() == Some(l) {
                        return Some(idx);
                    }
                }
                None => {
                    if f.loop_ctx.is_some() {
                        return Some(idx);
                    }
                    if allow_block && f.block_break.is_some() {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// The break target block + value slot for a scope frame (loop or block).
    fn break_target(&self, idx: usize) -> (BlockId, Option<LocalId>) {
        let f = &self.scopes[idx];
        if let Some(lc) = &f.loop_ctx {
            return (lc.break_bb, lc.break_val);
        }
        if let Some((slot, join)) = &f.block_break {
            return (*join, *slot);
        }
        // Fallback (should not happen): jump to current block.
        (self.cur, None)
    }

    // ===================================================================
    //  Capture binding helpers
    // ===================================================================

    /// Binds a single-name or multi-name capture to a payload operand.
    fn bind_capture(
        &mut self,
        capture: &Capture,
        payload: Operand,
        payload_ty: TypeId,
        span: Span,
    ) {
        if let Some(name) = capture.names.first() {
            if name.name == "_" {
                return;
            }
            let def = self.capture_name_def(name);
            let local = self.declare_local(def, payload_ty, span);
            self.assign(Place::local(local), Rvalue::Use(payload), span);
        }
    }

    /// The DefId of a capture name (declared at the capture's span).
    fn capture_name_def(&self, cap: &CaptureName) -> Option<DefId> {
        self.lo
            .resolved
            .defs
            .iter()
            .find(|d| matches!(d.kind, k2_resolve::DefKind::Capture) && d.span == cap.span)
            .map(|d| d.id)
    }

    /// The ok payload type of an error union (best effort).
    fn error_union_ok(&self, ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(ty) {
            Type::ErrorUnion { ok, .. } => *ok,
            _ => ty,
        }
    }

    /// The optional-inner type (best effort).
    fn optional_inner_pub(&self, ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(ty) {
            Type::Optional(inner) => *inner,
            _ => ty,
        }
    }

    /// Peels one pointer layer off a type (best effort).
    fn peel_ptr_pub(&self, ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(ty) {
            Type::Pointer { pointee, .. } => *pointee,
            _ => ty,
        }
    }
}
