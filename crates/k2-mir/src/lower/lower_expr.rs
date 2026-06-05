//! Expression lowering: `lower_into`, calls, intrinsics, constants, casts, and
//! the safety-check insertion helpers.
//!
//! [`FnBuilder::lower_into`] is the central dispatcher: it lowers an expression
//! so its value lands in a destination [`Place`]. Control-flow-as-expression
//! forms delegate to [`crate::lower::FnBuilder`]'s flow module; everything else —
//! literals, operators, field/index/deref reads, address-of, slicing, optional/
//! error-union construction and unwrap, casts, calls, and intrinsics — lives
//! here. Safety checks (bounds/overflow/divzero/narrowing/lockstep-length) are
//! inserted inline, gated by the build mode.

use super::*;

impl FnBuilder<'_, '_> {
    /// Lowers `e` so that its value is written into `dst`.
    pub(super) fn lower_into(&mut self, dst: Place, e: &Expr) {
        match e {
            // ---- constants -------------------------------------------------
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Char { .. }
            | Expr::Bool { .. }
            | Expr::Null { .. }
            | Expr::Undefined { .. }
            | Expr::ErrorLiteral { .. }
            | Expr::EnumLiteral { .. } => {
                let ty = self.type_at(e.span());
                if let Some(c) = self.const_of(e, ty) {
                    self.assign(dst, Rvalue::Use(Operand::Const(c)), e.span());
                } else {
                    self.assign(
                        dst,
                        Rvalue::Use(Operand::Const(Const::Undef { ty })),
                        e.span(),
                    );
                }
            }

            // ---- place reads ----------------------------------------------
            Expr::Ident { .. } | Expr::Deref { .. } => {
                if let Some(p) = self.try_lower_place(e) {
                    self.assign(dst, Rvalue::Use(Operand::Copy(p)), e.span());
                } else if let Some(init) = self.top_level_const_init(e) {
                    // A bare reference to a top-level (file-scope) *value* const
                    // has no local slot — inline its initializer expression so the
                    // const's value materializes, exactly as the namespaced
                    // `std.testing.allocator` member-access path does. Without this,
                    // `const eql = 5; ... .{eql}` would lower to `undef` and print
                    // the `<int>` placeholder instead of `5`.
                    self.lower_into(dst, &init);
                } else {
                    // No place, no value const: a type-denoting ident (`bool`,
                    // `u32`) or an opaque. Carry the DENOTED type when known so a
                    // type-valued intrinsic argument is concrete.
                    let ty = self.type_carrier_at(e.span());
                    self.assign(
                        dst,
                        Rvalue::Use(Operand::Const(Const::Undef { ty })),
                        e.span(),
                    );
                }
            }

            Expr::Field { base, field, span } => self.lower_field_into(dst, base, field, *span),

            Expr::Index { .. } => {
                if let Some(p) = self.try_lower_place(e) {
                    self.assign(dst, Rvalue::Use(Operand::Copy(p)), e.span());
                }
            }

            Expr::SliceExpr { base, lo, hi, span } => {
                self.lower_slice_expr_into(dst, base, lo, hi.as_deref(), *span)
            }

            // ---- operators -------------------------------------------------
            Expr::Binary { op, lhs, rhs, span } => {
                self.lower_binary_into(dst, *op, lhs, rhs, *span)
            }
            Expr::Unary { op, operand, span } => self.lower_unary_into(dst, *op, operand, *span),

            // ---- optional / error-union postfix ---------------------------
            Expr::Unwrap { base, span } => self.lower_unwrap_into(dst, base, *span),

            // ---- catch / orelse -------------------------------------------
            Expr::Catch {
                lhs,
                capture,
                rhs,
                span,
            } => self.lower_catch_into(dst, lhs, capture.as_deref(), rhs, *span),

            // ---- calls / intrinsics ---------------------------------------
            Expr::Call { callee, args, span } => self.lower_call_into(dst, callee, args, *span),

            // ---- builtins --------------------------------------------------
            Expr::Builtin { name, args, span } => self.lower_builtin_into(dst, name, args, *span),

            // ---- initializers ---------------------------------------------
            Expr::Init { ty, body, span } => self.lower_init_into(dst, ty.as_deref(), body, *span),

            // ---- comptime wrapper -----------------------------------------
            Expr::Comptime { inner, span } => {
                // The value was comptime-folded; inline the folded const if known,
                // else lower the inner expression (it has no runtime-only state).
                let ty = self.type_at(*span);
                if let Some(c) = self.comptime_span_const(*span, ty) {
                    self.assign(dst, Rvalue::Use(Operand::Const(c)), *span);
                } else {
                    self.lower_into(dst, inner);
                }
            }

            // ---- control flow as expression -------------------------------
            Expr::If { .. } => self.lower_if(e, Some(dst)),
            Expr::While { .. } => self.lower_while(e, Some(dst)),
            Expr::For { .. } => self.lower_for(e, Some(dst)),
            Expr::Switch { .. } => self.lower_switch(e, Some(dst)),
            Expr::Block { label, body, .. } => {
                self.lower_block_expr(label.as_deref(), body, Some(dst))
            }

            Expr::Unreachable { span } => self.lower_unreachable(*span),

            // ---- type-denoting expressions (folded at comptime) -----------
            // A type expression appearing in value position was comptime-folded;
            // it never produces a runtime value. Emit an undef of its type.
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
                // The undef carrier is the DENOTED type when the checker recorded
                // one (so a type-valued intrinsic argument like the `[]const u8` in
                // `b.option([]const u8, ...)` carries the concrete type), else the
                // meta-type `type`.
                let ty = self.type_carrier_at(e.span());
                self.assign(
                    dst,
                    Rvalue::Use(Operand::Const(Const::Undef { ty })),
                    e.span(),
                );
            }
        }
    }

    /// Emits `dst = rvalue;`.
    pub(super) fn assign(&mut self, dst: Place, rvalue: Rvalue, span: Span) {
        self.emit(Statement::Assign {
            place: dst,
            rvalue,
            span,
        });
    }

    // ===================================================================
    //  Field / index / slice
    // ===================================================================

    /// Lowers a `base.field` read into `dst`, dispatching on the member kind.
    fn lower_field_into(&mut self, dst: Place, base: &Expr, field: &str, span: Span) {
        // `Set.Member` on a named error-set type is an error *value*, even though
        // the checker leaves it `Deferred` (the base is a `type`).
        if self.error_set_member(base, field).is_some() {
            let tag = self.lo.err_tag(field);
            let ty = self.type_at(span);
            self.assign(
                dst,
                Rvalue::Use(Operand::Const(Const::ErrVal { tag, ty })),
                span,
            );
            return;
        }
        match self.member_at(span) {
            Some(MemberRes::Field(_))
            | Some(MemberRes::PackedField(..))
            | Some(MemberRes::BuiltinField) => {
                if let Some(p) = self.try_lower_place(&Expr::Field {
                    base: Box::new(base.clone()),
                    field: field.to_string(),
                    span,
                }) {
                    self.assign(dst, Rvalue::Use(Operand::Copy(p)), span);
                }
            }
            Some(MemberRes::Variant(idx)) => {
                let ty = self.type_at(span);
                self.assign(
                    dst,
                    Rvalue::Use(Operand::Const(Const::EnumVal { variant: idx, ty })),
                    span,
                );
            }
            Some(MemberRes::ErrorMember) => {
                let tag = self.lo.err_tag(field);
                let ty = self.type_at(span);
                self.assign(
                    dst,
                    Rvalue::Use(Operand::Const(Const::ErrVal { tag, ty })),
                    span,
                );
            }
            Some(MemberRes::Decl(d)) if self.lo.value_const_inits.contains_key(&d) => {
                // A namespaced *value* const (`std.testing.allocator`): inline its
                // initializer expression instead of emitting an `@std.<member>`
                // intrinsic the VM cannot dispatch. The initializer is itself a
                // value-producing expression (e.g. `@allocHandle(@allocId(5, 0))`),
                // so lowering it yields the right runtime value directly.
                let init = self.lo.value_const_inits[&d].clone();
                self.lower_into(dst, &init);
            }
            Some(MemberRes::Decl(_)) | Some(MemberRes::Deferred) | None => {
                // A member constant inline, or a Deferred member read (a field
                // access on std/sys whose value is opaque) -> emit an intrinsic
                // read so the VM can resolve it.
                let path = self.build_intrinsic_path(base, &[field.to_string()], false);
                if let Some(path) = path {
                    let ty = self.type_at(span);
                    self.assign(
                        dst,
                        Rvalue::Intrinsic {
                            path,
                            args: Vec::new(),
                            ty,
                        },
                        span,
                    );
                } else {
                    let ty = self.type_at(span);
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
        }
    }

    /// Lowers `base[lo..hi]` into `dst`: a bounds-checked sub-slice.
    fn lower_slice_expr_into(
        &mut self,
        dst: Place,
        base: &Expr,
        lo: &Expr,
        hi: Option<&Expr>,
        span: Span,
    ) {
        let slice_ty = self.type_at(span);
        let usize_ty = self.lo.typed.arena.t_usize();
        let base_place = self.lower_place_autoderef_pub(base);
        let len = self.slice_len_operand(&base_place, base);
        let lo_op = self.lower_operand(lo, usize_ty);
        let hi_op = match hi {
            Some(h) => self.lower_operand(h, usize_ty),
            None => len.clone(),
        };
        // Range check `lo <= hi <= len` in safe builds.
        if self.lo.mode.checks_enabled() {
            self.emit(Statement::Check(SafetyCheck {
                kind: CheckKind::SliceRange {
                    lo: lo_op.clone(),
                    hi: hi_op.clone(),
                    len,
                },
                span,
            }));
        }
        // ptr = base.ptr + lo; len = hi - lo. We model the resulting slice via
        // MakeSlice with the base place's ptr meta and the computed length.
        let ptr_ty = self.slice_ptr_type(slice_ty);
        let ptr_meta = base_place.project(Proj::SliceMeta {
            which: SliceMeta::Ptr,
            ty: ptr_ty,
        });
        let new_len_tmp = self.new_temp(usize_ty, span);
        self.assign(
            Place::local(new_len_tmp),
            Rvalue::Binary {
                op: BinOp::Sub,
                lhs: hi_op,
                rhs: lo_op.clone(),
                ty: usize_ty,
            },
            span,
        );
        self.assign(
            dst,
            Rvalue::MakeSlice {
                ptr: Operand::Copy(ptr_meta),
                // The sub-slice starts at element `lo` of the base.
                offset: lo_op,
                len: Operand::local(new_len_tmp),
                ty: slice_ty,
            },
            span,
        );
    }

    /// The `*elem` pointer type for a slice type (best effort).
    fn slice_ptr_type(&mut self, slice_ty: TypeId) -> TypeId {
        if let Type::Slice { is_const, elem } = self.lo.typed.arena.get(slice_ty).clone() {
            self.lo.typed.arena.ptr(is_const, elem)
        } else {
            self.lo.typed.arena.t_usize()
        }
    }

    /// An operand for `place.len` (for a slice) or the comptime array length.
    pub(super) fn slice_len_operand(&mut self, place: &Place, base: &Expr) -> Operand {
        let bty = self.type_at(base.span());
        let usize_ty = self.lo.typed.arena.t_usize();
        match self.lo.typed.arena.get(self.peel_ptr(bty)) {
            Type::Array { len, .. } => {
                if let k2_types::ArrayLen::Known(n) = len {
                    return Operand::Const(Const::Int {
                        value: *n as i128,
                        ty: usize_ty,
                    });
                }
                // Deferred/inferred length: read .len like a slice.
                Operand::Copy(place.project(Proj::SliceMeta {
                    which: SliceMeta::Len,
                    ty: usize_ty,
                }))
            }
            // A `@Vector(N, T)` is stored inline with NO `{ptr, len}` header, so a
            // `.len` projection over it would (on native) read the vector storage /
            // adjacent stack as a bogus length and spuriously trap the bounds
            // check. Its length is a comptime constant — materialize it directly,
            // exactly like a known-length array, so the bounds check uses the true
            // lane count on both backends (spec §02).
            Type::Vector { len, .. } => Operand::Const(Const::Int {
                value: *len as i128,
                ty: usize_ty,
            }),
            _ => Operand::Copy(place.project(Proj::SliceMeta {
                which: SliceMeta::Len,
                ty: usize_ty,
            })),
        }
    }

    /// Peels one pointer layer off a type.
    pub(super) fn peel_ptr(&self, ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(ty) {
            Type::Pointer { pointee, .. } => *pointee,
            _ => ty,
        }
    }

    /// Public wrapper around the private auto-deref place lowering.
    pub(super) fn lower_place_autoderef_pub(&mut self, base: &Expr) -> Place {
        self.lower_place_autoderef(base)
    }

    // ===================================================================
    //  Operators
    // ===================================================================

    /// Lowers a binary operation into `dst`.
    fn lower_binary_into(&mut self, dst: Place, op: AstBinOp, lhs: &Expr, rhs: &Expr, span: Span) {
        match op {
            AstBinOp::And => self.lower_short_circuit(dst, lhs, rhs, span, true),
            AstBinOp::Or => self.lower_short_circuit(dst, lhs, rhs, span, false),
            AstBinOp::Orelse => self.lower_orelse_into(dst, lhs, rhs, span),
            AstBinOp::Concat | AstBinOp::ErrSetMerge => {
                // `++` (comptime concat) / `||` (error-set merge) are comptime;
                // their value was folded. Inline the folded const if known.
                let ty = self.type_at(span);
                if let Some(c) = self.comptime_span_const(span, ty) {
                    self.assign(dst, Rvalue::Use(Operand::Const(c)), span);
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            _ => {
                let ty = self.type_at(span);
                // Element-wise vector arithmetic/compare (spec §02). A binary op on
                // two `@Vector(N,T)` (or producing `@Vector(N,bool)` for a compare)
                // is desugared here into an array aggregate of per-lane scalar ops,
                // reusing all the existing array machinery on BOTH backends.
                let lty = self.type_at(lhs.span());
                if matches!(self.lo.typed.arena.get(lty), Type::Vector { .. }) {
                    self.lower_vector_binary_into(dst, op, (lhs, rhs), ty, span);
                    return;
                }
                let bin = map_binop(op);
                let rty = self.type_at(rhs.span());
                let l = self.lower_operand(lhs, lty);
                let r = self.lower_operand(rhs, rty);
                let rv = self.checked_binary(bin, l, r, ty, span);
                self.assign(dst, rv, span);
            }
        }
    }

    /// Lowers an element-wise vector binary op into `dst` by building an array
    /// aggregate whose `i`-th element is the scalar op on lane `i` of each operand
    /// (spec §02). Vector arithmetic is per-lane WRAPPING (no overflow check), so
    /// the scalar binary is emitted check-free. The result vector type is `res_ty`
    /// (the same vector for arithmetic, `@Vector(N, bool)` for a comparison).
    fn lower_vector_binary_into(
        &mut self,
        dst: Place,
        op: AstBinOp,
        operands: (&Expr, &Expr),
        res_ty: TypeId,
        span: Span,
    ) {
        let (lhs, rhs) = operands;
        let vec_ty = self.type_at(lhs.span());
        let Type::Vector { len, elem } = self.lo.typed.arena.get(vec_ty) else {
            return;
        };
        let len = *len;
        let elem = *elem;
        let res_elem = match self.lo.typed.arena.get(res_ty) {
            Type::Vector { elem, .. } => *elem,
            _ => elem,
        };
        let bin = map_binop(op);
        // Materialize both operands into temps so each lane reads a stable place.
        let lop = self.lower_operand(lhs, vec_ty);
        let rop = self.lower_operand(rhs, vec_ty);
        let lt = self.materialize_operand(lop, vec_ty, span);
        let rt = self.materialize_operand(rop, vec_ty, span);
        let usize_ty = self.lo.typed.arena.t_usize();
        let mut fields = Vec::with_capacity(len as usize);
        for i in 0..len {
            let idx = Operand::Const(Const::Int {
                value: i as i128,
                ty: usize_ty,
            });
            let li = Operand::Copy(Place::local(lt).project(Proj::Index {
                index: idx.clone(),
                ty: elem,
            }));
            let ri = Operand::Copy(Place::local(rt).project(Proj::Index {
                index: idx,
                ty: elem,
            }));
            let lane = self.new_temp(res_elem, span);
            // Check-free per-lane op (vector arithmetic wraps).
            self.assign(
                Place::local(lane),
                Rvalue::Binary {
                    op: bin,
                    lhs: li,
                    rhs: ri,
                    ty: res_elem,
                },
                span,
            );
            fields.push(Operand::local(lane));
        }
        self.assign(
            dst,
            Rvalue::Aggregate {
                kind: AggKind::Array,
                fields,
                ty: res_ty,
            },
            span,
        );
    }

    /// Lowers `@reduce(.Op, vec)` into `dst` (spec §02): a left fold of the lanes
    /// with the operation. `.Add/.Mul/.And/.Or/.Xor` fold with the matching
    /// [`BinOp`]; `.Min/.Max` fold with a per-step `lt` compare + select branch.
    fn lower_reduce_into(&mut self, dst: Place, args: &[Expr], elem_res: TypeId, span: Span) {
        let [op_lit, vec_e] = args else {
            self.assign(
                dst,
                Rvalue::Use(Operand::Const(Const::Undef { ty: elem_res })),
                span,
            );
            return;
        };
        let vec_ty = self.type_at(vec_e.span());
        let Type::Vector { len, elem } = self.lo.typed.arena.get(vec_ty) else {
            self.assign(
                dst,
                Rvalue::Use(Operand::Const(Const::Undef { ty: elem_res })),
                span,
            );
            return;
        };
        let len = *len;
        let elem = *elem;
        let op_name = match op_lit {
            Expr::EnumLiteral { name, .. } => name.clone(),
            _ => String::new(),
        };
        let vop = self.lower_operand(vec_e, vec_ty);
        let src = self.materialize_operand(vop, vec_ty, span);
        let usize_ty = self.lo.typed.arena.t_usize();
        let lane = |this: &mut Self, i: u32| {
            let idx = Operand::Const(Const::Int {
                value: i as i128,
                ty: usize_ty,
            });
            let _ = this;
            Operand::Copy(Place::local(src).project(Proj::Index {
                index: idx,
                ty: elem,
            }))
        };
        if len == 0 {
            self.assign(
                dst,
                Rvalue::Use(Operand::Const(Const::Undef { ty: elem_res })),
                span,
            );
            return;
        }
        // The accumulator starts at lane 0.
        let acc = self.new_temp(elem, span);
        let l0 = lane(self, 0);
        self.assign(Place::local(acc), Rvalue::Use(l0), span);
        let fold_op = match op_name.as_str() {
            "Add" => Some(BinOp::Add),
            "Mul" => Some(BinOp::Mul),
            "And" => Some(BinOp::BitAnd),
            "Or" => Some(BinOp::BitOr),
            "Xor" => Some(BinOp::BitXor),
            _ => None,
        };
        for i in 1..len {
            let li = lane(self, i);
            if let Some(bin) = fold_op {
                let next = self.new_temp(elem, span);
                self.assign(
                    Place::local(next),
                    Rvalue::Binary {
                        op: bin,
                        lhs: Operand::local(acc),
                        rhs: li,
                        ty: elem,
                    },
                    span,
                );
                self.assign(Place::local(acc), Rvalue::Use(Operand::local(next)), span);
            } else if op_name == "Min" || op_name == "Max" {
                // acc = (lane < acc) == want_min ? lane : acc.
                let bool_ty = self.lo.typed.arena.t_bool();
                let cond = self.new_temp(bool_ty, span);
                self.assign(
                    Place::local(cond),
                    Rvalue::Binary {
                        op: BinOp::Lt,
                        lhs: li.clone(),
                        rhs: Operand::local(acc),
                        ty: bool_ty,
                    },
                    span,
                );
                let take = self.new_block();
                let join = self.new_block();
                // For Min: take `lane` when lane < acc. For Max: take `lane` when
                // !(lane < acc) i.e. lane >= acc.
                let (then_bb, else_bb) = if op_name == "Min" {
                    (take, join)
                } else {
                    (join, take)
                };
                self.set_term(Terminator::Branch {
                    cond: Operand::local(cond),
                    then_bb,
                    else_bb,
                });
                self.cur = take;
                self.assign(Place::local(acc), Rvalue::Use(li), span);
                self.set_term(Terminator::Goto(join));
                self.cur = join;
            }
        }
        self.assign(dst, Rvalue::Use(Operand::local(acc)), span);
    }

    /// Copies an operand into a fresh temp local of `ty` and returns it, so a
    /// per-lane index projection reads a stable addressable place.
    fn materialize_operand(&mut self, op: Operand, ty: TypeId, span: Span) -> LocalId {
        if let Operand::Copy(p) = &op {
            if p.proj.is_empty() {
                return p.base;
            }
        }
        let tmp = self.new_temp(ty, span);
        self.assign(Place::local(tmp), Rvalue::Use(op), span);
        tmp
    }

    /// The effective type to drive an `&operand` (`AddrOf`) lowering. The checker
    /// types the address expression by its *natural* address (`*[N]T` for `&array`,
    /// `*T` for `&local`), losing the array→slice coercion that the surrounding
    /// context requests. When the destination is a bare local whose declared type
    /// is a `Slice`, that slice type is the real target — return it so `&array`
    /// produces a fat `{ptr, len}` slice (`MakeSlice`) rather than a single pointer.
    /// Otherwise fall back to the expression's own type at `span`.
    fn addr_of_target_ty(&self, dst: &Place, span: Span) -> TypeId {
        if dst.proj.is_empty() {
            let dty = self.func.locals[dst.base.index()].ty;
            if matches!(self.lo.typed.arena.get(dty), Type::Slice { .. }) {
                return dty;
            }
        }
        self.type_at(span)
    }

    /// Lowers a unary operation into `dst`.
    fn lower_unary_into(&mut self, dst: Place, op: AstUnOp, operand: &Expr, span: Span) {
        match op {
            AstUnOp::AddrOf => {
                // The type of the `&operand` expression itself. The checker records
                // this as the *natural* address type (`*[N]T` for `&array`), even
                // when the value is being coerced into a slice parameter/destination
                // — the slice-ness lives only in the DESTINATION's type. So prefer
                // the destination local's type when it is a `Slice`: a bare
                // `&array` passed to a `[]T` parameter must materialize a real fat
                // `{ptr, len}` slice (a `MakeSlice`), not a single pointer the native
                // backend would marshal as one register and then read a garbage
                // `.len` from (yielding e.g. a `for (xs) |x|` loop that sums to 0).
                let ty = self.addr_of_target_ty(&dst, span);
                // `&.{}` — the canonical empty slice (`&[0]T` coerced to `[]T`).
                // It points at no storage, so it is the empty-slice literal, not
                // a reference to a stack temporary.
                if is_empty_init(operand)
                    && matches!(self.lo.typed.arena.get(ty), Type::Slice { .. })
                {
                    self.assign(
                        dst,
                        Rvalue::Use(Operand::Const(Const::EmptySlice { ty })),
                        span,
                    );
                    return;
                }
                // `&array` coerced to a slice -> a slice over the whole array.
                if matches!(self.lo.typed.arena.get(ty), Type::Slice { .. }) {
                    let place = self.lower_place_autoderef_pub(operand);
                    let base_ty = self.type_at(operand.span());
                    if matches!(
                        self.lo.typed.arena.get(self.peel_ptr(base_ty)),
                        Type::Array { .. }
                    ) {
                        let len = self.slice_len_operand(&place, operand);
                        let ptr_ty = self.slice_ptr_type(ty);
                        let ptr = place.project(Proj::SliceMeta {
                            which: SliceMeta::Ptr,
                            ty: ptr_ty,
                        });
                        self.mark_address_taken(place.base);
                        let usize_ty = self.lo.typed.arena.t_usize();
                        self.assign(
                            dst,
                            Rvalue::MakeSlice {
                                ptr: Operand::Copy(ptr),
                                // A whole-array view starts at element 0.
                                offset: Operand::Const(Const::Int {
                                    value: 0,
                                    ty: usize_ty,
                                }),
                                len,
                                ty,
                            },
                            span,
                        );
                        return;
                    }
                }
                let place = self.lower_place(operand);
                self.mark_address_taken(place.base);
                let is_const = matches!(
                    self.lo.typed.arena.get(ty),
                    Type::Pointer { is_const: true, .. }
                );
                self.assign(
                    dst,
                    Rvalue::Ref {
                        place,
                        is_const,
                        ty,
                    },
                    span,
                );
            }
            AstUnOp::Try => {
                let val = self.lower_try(operand, span);
                self.assign(dst, Rvalue::Use(val), span);
            }
            AstUnOp::Neg => {
                let ty = self.type_at(span);
                let oty = self.type_at(operand.span());
                let o = self.lower_operand(operand, oty);
                // Negation overflow check for signed types in safe builds.
                if self.lo.mode.checks_enabled() && self.is_signed_int(ty) {
                    self.emit(Statement::Check(SafetyCheck {
                        kind: CheckKind::NegOverflow { a: o.clone(), ty },
                        span,
                    }));
                }
                self.assign(
                    dst,
                    Rvalue::Unary {
                        op: UnOp::Neg,
                        operand: o,
                        ty,
                    },
                    span,
                );
            }
            AstUnOp::BitNot | AstUnOp::Not => {
                let ty = self.type_at(span);
                let oty = self.type_at(operand.span());
                let o = self.lower_operand(operand, oty);
                let uop = if matches!(op, AstUnOp::Not) {
                    UnOp::Not
                } else {
                    UnOp::BitNot
                };
                self.assign(
                    dst,
                    Rvalue::Unary {
                        op: uop,
                        operand: o,
                        ty,
                    },
                    span,
                );
            }
        }
    }

    /// `true` if `ty` is a signed integer type.
    pub(super) fn is_signed_int(&self, ty: TypeId) -> bool {
        matches!(self.lo.typed.arena.get(ty), Type::Int { signed: true, .. })
    }

    /// `true` if `ty` is any sized integer type (overflow-checkable).
    pub(super) fn is_sized_int(&self, ty: TypeId) -> bool {
        matches!(self.lo.typed.arena.get(ty), Type::Int { .. })
    }

    /// Builds a binary rvalue, inserting overflow / divide-by-zero checks for the
    /// arithmetic operators in safe builds.
    pub(super) fn checked_binary(
        &mut self,
        op: BinOp,
        lhs: Operand,
        rhs: Operand,
        ty: TypeId,
        span: Span,
    ) -> Rvalue {
        if self.lo.mode.checks_enabled() && self.is_sized_int(ty) {
            match op {
                BinOp::Add => self.emit_overflow_check(ArithOp::Add, &lhs, &rhs, ty, span),
                BinOp::Sub => self.emit_overflow_check(ArithOp::Sub, &lhs, &rhs, ty, span),
                BinOp::Mul => self.emit_overflow_check(ArithOp::Mul, &lhs, &rhs, ty, span),
                BinOp::Div | BinOp::Rem => {
                    self.emit(Statement::Check(SafetyCheck {
                        kind: CheckKind::DivByZero { b: rhs.clone(), ty },
                        span,
                    }));
                    // Signed `/`/`%` also trap on `type-MIN op -1`, whose true
                    // result (`-MIN`) does not fit the type.
                    if self.is_signed_int(ty) {
                        self.emit(Statement::Check(SafetyCheck {
                            kind: CheckKind::DivOverflow {
                                a: lhs.clone(),
                                b: rhs.clone(),
                                ty,
                            },
                            span,
                        }));
                    }
                }
                _ => {}
            }
        }
        Rvalue::Binary { op, lhs, rhs, ty }
    }

    /// Emits an integer-overflow safety check for `a op b`.
    fn emit_overflow_check(
        &mut self,
        op: ArithOp,
        a: &Operand,
        b: &Operand,
        ty: TypeId,
        span: Span,
    ) {
        self.emit(Statement::Check(SafetyCheck {
            kind: CheckKind::AddOverflow {
                op,
                a: a.clone(),
                b: b.clone(),
                ty,
            },
            span,
        }));
    }

    /// Emits a bounds check before an index access (safe builds only).
    pub(super) fn emit_bounds_check(
        &mut self,
        base_place: &Place,
        base: &Expr,
        idx: &Operand,
        span: Span,
    ) {
        if !self.lo.mode.checks_enabled() {
            return;
        }
        let len = self.slice_len_operand(base_place, base);
        self.emit(Statement::Check(SafetyCheck {
            kind: CheckKind::Bounds {
                index: idx.clone(),
                len,
            },
            span,
        }));
    }

    // ===================================================================
    //  Short-circuit and/or, orelse
    // ===================================================================

    /// Lowers `a and b` (`is_and`) / `a or b` into `dst` as branches, so `b` is
    /// only evaluated on the path that needs it.
    fn lower_short_circuit(
        &mut self,
        dst: Place,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        is_and: bool,
    ) {
        let bool_ty = self.lo.typed.arena.t_bool();
        let a = self.lower_operand(lhs, bool_ty);
        let eval_b = self.new_block();
        let short = self.new_block();
        let join = self.new_block();
        // `and`: if a -> eval_b else short(false). `or`: if a -> short(true) else eval_b.
        if is_and {
            self.set_term(Terminator::Branch {
                cond: a,
                then_bb: eval_b,
                else_bb: short,
            });
        } else {
            self.set_term(Terminator::Branch {
                cond: a,
                then_bb: short,
                else_bb: eval_b,
            });
        }
        // eval_b: dst = b; goto join.
        self.cur = eval_b;
        let b = self.lower_operand(rhs, bool_ty);
        self.assign(dst.clone(), Rvalue::Use(b), span);
        self.set_term(Terminator::Goto(join));
        // short: dst = (false for and / true for or); goto join.
        self.cur = short;
        self.assign(dst, Rvalue::Use(Operand::Const(Const::Bool(!is_and))), span);
        self.set_term(Terminator::Goto(join));
        self.cur = join;
    }

    /// Lowers `opt orelse default` into `dst` (default only on the null path).
    fn lower_orelse_into(&mut self, dst: Place, opt: &Expr, default: &Expr, span: Span) {
        let opt_ty = self.type_at(opt.span());
        let opt_op = self.lower_operand(opt, opt_ty);
        let is_null = self.new_temp(self.lo.typed.arena.t_bool(), span);
        self.assign(
            Place::local(is_null),
            Rvalue::Discriminant {
                operand: opt_op.clone(),
                kind: DiscrKind::Optional,
            },
            span,
        );
        let null_bb = self.new_block();
        let some_bb = self.new_block();
        let join = self.new_block();
        self.set_term(Terminator::Branch {
            cond: Operand::local(is_null),
            then_bb: null_bb,
            else_bb: some_bb,
        });
        // some: dst = payload(opt).
        self.cur = some_bb;
        let payload_ty = self.optional_inner(opt_ty);
        let payload = self.operand_payload(opt_op, payload_ty, DiscrKind::Optional);
        self.assign(dst.clone(), Rvalue::Use(payload), span);
        self.set_term(Terminator::Goto(join));
        // null: dst = default (only evaluated here).
        self.cur = null_bb;
        self.lower_into(dst, default);
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        self.cur = join;
    }

    /// The inner type of an optional (best effort).
    fn optional_inner(&self, opt_ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(opt_ty) {
            Type::Optional(inner) => *inner,
            _ => opt_ty,
        }
    }

    /// Builds an operand reading the payload of an optional/error-union operand.
    /// Materializes the operand into a temp so we can project a Payload.
    pub(super) fn operand_payload(
        &mut self,
        operand: Operand,
        payload_ty: TypeId,
        _k: DiscrKind,
    ) -> Operand {
        match operand {
            Operand::Copy(p) => Operand::Copy(p.project(Proj::Payload { ty: payload_ty })),
            Operand::Const(_) => {
                // Materialize the constant into a temp first.
                let agg_ty = payload_ty;
                let tmp = self.new_temp(agg_ty, Span::default());
                self.assign(Place::local(tmp), Rvalue::Use(operand), Span::default());
                Operand::Copy(Place::local(tmp).project(Proj::Payload { ty: payload_ty }))
            }
        }
    }

    // ===================================================================
    //  Optional unwrap `.?`
    // ===================================================================

    /// Lowers `base.?` into `dst`, trapping on null in safe builds.
    fn lower_unwrap_into(&mut self, dst: Place, base: &Expr, span: Span) {
        let opt_ty = self.type_at(base.span());
        let opt_op = self.lower_operand(base, opt_ty);
        let payload_ty = self.type_at(span);
        if self.lo.mode.checks_enabled() {
            let is_null = self.new_temp(self.lo.typed.arena.t_bool(), span);
            self.assign(
                Place::local(is_null),
                Rvalue::Discriminant {
                    operand: opt_op.clone(),
                    kind: DiscrKind::Optional,
                },
                span,
            );
            let panic_bb = self.new_block();
            self.func.blocks[panic_bb.index()].is_panic = true;
            self.func.blocks[panic_bb.index()].term = Terminator::Trap {
                reason: TrapReason::Panic,
            };
            let ok_bb = self.new_block();
            self.set_term(Terminator::Branch {
                cond: Operand::local(is_null),
                then_bb: panic_bb,
                else_bb: ok_bb,
            });
            self.cur = ok_bb;
        }
        let payload = self.operand_payload(opt_op, payload_ty, DiscrKind::Optional);
        self.assign(dst, Rvalue::Use(payload), span);
    }

    // ===================================================================
    //  Calls & intrinsics
    // ===================================================================

    /// Lowers a call `callee(args)` into `dst`.
    pub(super) fn lower_call_into(&mut self, dst: Place, callee: &Expr, args: &[Expr], span: Span) {
        let ret_ty = self.type_at(span);
        // The v0.11 `spawn` rewrite: `exec.spawn(work, args)` / `loop.spawn(...)`
        // is lowered DIRECTLY to the `@schedSpawn` scheduler intrinsic with `work`
        // resolved to its function reference, then wrapped in the result handle
        // (`Task`/`Future`) struct. This sidesteps passing a function by value
        // through a generic method body (the MIR has no indirect calls / first-
        // class fn pointers); the handle struct is single-field (`id: u32`), so the
        // wrap is a one-field aggregate of the call's own result type.
        if self.try_lower_spawn(dst.clone(), callee, args, span, ret_ty) {
            return;
        }
        // A still-Deferred member call -> intrinsic (std/sys/build boundary).
        if let Some((path, recv_args)) = self.try_intrinsic_call(callee, args) {
            let mut arg_ops = recv_args;
            for a in args {
                let aty = self.type_at(a.span());
                arg_ops.push(self.lower_call_arg(a, aty));
            }
            self.assign(
                dst,
                Rvalue::Intrinsic {
                    path,
                    args: arg_ops,
                    ty: ret_ty,
                },
                span,
            );
            return;
        }
        // A direct or method call to a concrete, monomorphizable fn.
        if let Some((fid, recv_args, params_skip)) = self.resolve_direct_call(callee, span) {
            let mut arg_ops = recv_args;
            // Map explicit args to the callee's parameter types (after receiver).
            let param_types = self.callee_param_types(fid);
            for (i, a) in args.iter().enumerate() {
                let pty = param_types
                    .get(i + params_skip)
                    .copied()
                    .unwrap_or_else(|| self.type_at(a.span()));
                arg_ops.push(self.lower_call_arg(a, pty));
            }
            self.assign(
                dst,
                Rvalue::Call {
                    func: fid,
                    args: arg_ops,
                    ty: ret_ty,
                },
                span,
            );
            return;
        }
        // Fallback: an opaque call we could not resolve (e.g. a fn-typed value
        // through a Deferred base). Lower as an intrinsic on the callee chain so
        // lowering never fails.
        if let Some(path) = self.build_intrinsic_path_from_call(callee) {
            let mut arg_ops = Vec::new();
            for a in args {
                let aty = self.type_at(a.span());
                arg_ops.push(self.lower_call_arg(a, aty));
            }
            self.assign(
                dst,
                Rvalue::Intrinsic {
                    path,
                    args: arg_ops,
                    ty: ret_ty,
                },
                span,
            );
            return;
        }
        // Truly unknown: emit undef so lowering is total.
        self.assign(
            dst,
            Rvalue::Use(Operand::Const(Const::Undef { ty: ret_ty })),
            span,
        );
    }

    /// Lowers a call argument. A `type` argument to a generic is passed as an
    /// Undef carrying the TypeId (so the VM knows the element layout); everything
    /// else is an ordinary operand.
    fn lower_call_arg(&mut self, a: &Expr, ty: TypeId) -> Operand {
        // A bare type name / type-valued expression used as an argument.
        let aty = self.type_at(a.span());
        if matches!(self.lo.typed.arena.get(aty), Type::TypeType) {
            // Pass the denoted type (if known) as an Undef carrying its TypeId.
            if let Some(&t) = self
                .lo
                .typed
                .type_valued_spans
                .get(&(a.span().start, a.span().end))
            {
                return Operand::Const(Const::Undef { ty: t });
            }
            // A predeclared type name: use the recorded type.
            return Operand::Const(Const::Undef { ty: aty });
        }
        self.lower_operand(a, ty)
    }

    /// Lowers a *function-reference* argument (the work fn passed to
    /// `Executor.spawn`/`Loop.spawn` via `@schedSpawn`). A bare function identifier
    /// is resolved to its `DefId`, enqueued for monomorphization (as a plain
    /// instantiation — spawned work is non-generic), and emitted as an `FnRef`
    /// const carrying the callee's [`FnId`]. Anything that is not a resolvable
    /// function name falls back to `undef` (lowering stays total).
    fn lower_fn_ref_arg(&mut self, a: &Expr) -> Operand {
        self.try_fn_ref(a).unwrap_or_else(|| {
            Operand::Const(Const::Undef {
                ty: self.type_at(a.span()),
            })
        })
    }

    /// Resolves an expression naming a function to an `FnRef` operand, or `None` if
    /// it is not a resolvable (non-generic) function identifier.
    fn try_fn_ref(&mut self, a: &Expr) -> Option<Operand> {
        let Expr::Ident { span, .. } = a else {
            return None;
        };
        let def = self.resolved_def(*span)?;
        if !self.lo.fn_items.contains_key(&def) {
            return None;
        }
        let fid = self.lo.enqueue(InstId::plain(def));
        Some(Operand::Const(Const::FnRef(fid)))
    }

    /// Rewrites the v0.11 concurrency-handle methods to direct scheduler
    /// intrinsics, since `Executor.spawn`/`Loop.spawn` take a function *by value*
    /// (which the MIR cannot pass as a first-class pointer) and the resulting
    /// `Task`/`Future` is realized as a bare `u32` fiber id. Recognized shapes:
    ///
    /// - `recv.spawn(work, args)` -> `@schedSpawn(FnRef(work), args)` (the id).
    /// - `task.join()` / `task.result(T)` -> `@schedAwait(task)`.
    /// - `future.await(loop, T)` -> `@schedAwait(future)`.
    ///
    /// Returns `true` if it matched and lowered the call. The `await`/`result`/
    /// `join`/`spawn` member names plus an operand-shaped receiver make a false
    /// match vanishingly unlikely; if a future user type reuses these names this
    /// guard can be tightened to the concrete std handle types.
    fn try_lower_spawn(
        &mut self,
        dst: Place,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        ret_ty: TypeId,
    ) -> bool {
        let Expr::Field { base, field, .. } = callee else {
            return false;
        };
        match field.as_str() {
            // `recv.spawn(work, args)` -> `@schedSpawn(FnRef(work), args)`.
            "spawn" if args.len() == 2 => {
                let Some(fn_ref) = self.try_fn_ref(&args[0]) else {
                    return false;
                };
                let args_ty = self.type_at(args[1].span());
                let args_op = self.lower_call_arg(&args[1], args_ty);
                self.emit_sched_intrinsic(dst, "schedSpawn", vec![fn_ref, args_op], ret_ty, span);
                true
            }
            // `task.join()` / `task.result(T)` -> `@schedAwait(task)`.
            "join" | "result" if self.receiver_is_handle(base) => {
                let recv = self.lower_handle_receiver(base);
                self.emit_sched_intrinsic(dst, "schedAwait", vec![recv], ret_ty, span);
                true
            }
            // `future.await(loop, T)` -> `@schedAwait(future)`.
            "await" if self.receiver_is_handle(base) => {
                let recv = self.lower_handle_receiver(base);
                self.emit_sched_intrinsic(dst, "schedAwait", vec![recv], ret_ty, span);
                true
            }
            _ => false,
        }
    }

    /// `true` if `base` denotes a concurrency-handle receiver (a `u32` fiber id or
    /// a still-`deferred` value — the shape `Task`/`Future` lowers to). This keeps
    /// the `join`/`result`/`await` rewrite from firing on an unrelated method of
    /// the same name on a concrete non-handle type.
    fn receiver_is_handle(&self, base: &Expr) -> bool {
        let ty = self.type_at(base.span());
        matches!(
            self.lo.typed.arena.get(ty),
            Type::Deferred | Type::Int { .. }
        )
    }

    /// Lowers a concurrency-handle receiver (the `Task`/`Future` value) to its
    /// `u32` fiber-id operand.
    fn lower_handle_receiver(&mut self, base: &Expr) -> Operand {
        let ty = self.type_at(base.span());
        self.lower_operand(base, ty)
    }

    /// Emits a scheduler `@builtin` intrinsic call into `dst`.
    fn emit_sched_intrinsic(
        &mut self,
        dst: Place,
        name: &str,
        args: Vec<Operand>,
        ret_ty: TypeId,
        span: Span,
    ) {
        self.assign(
            dst,
            Rvalue::Intrinsic {
                path: IntrinsicPath {
                    root: IntrinsicRoot::Builtin(name.to_string()),
                    members: Vec::new(),
                    is_call: true,
                },
                args,
                ty: ret_ty,
            },
            span,
        );
    }

    /// The parameter types of a monomorphized callee.
    ///
    /// When the callee is already lowered, its locals carry the resolved param
    /// types directly. But `enqueue` only RESERVES a placeholder (empty `params`,
    /// empty `locals`) and defers the body to the worklist, so a forward/recursive
    /// reference — the common `main` calls `sumArr` before `sumArr` is lowered —
    /// sees no params and would mistype every argument by its own expression type.
    /// That mistypes a `&array` argument passed to a `[]T` parameter as `*[N]T`
    /// (a single pointer) instead of a fat `{ptr, len}` slice, so the native
    /// backend marshals one register and the callee reads a garbage `.len`. To be
    /// robust against lowering order, resolve the param types from the callee's AST
    /// signature (via `param_type`) when its lowered locals are not yet present.
    fn callee_param_types(&self, fid: FnId) -> Vec<TypeId> {
        let f = &self.lo.funcs[fid.index()];
        if !f.params.is_empty() {
            return f.params.iter().map(|p| f.locals[p.index()].ty).collect();
        }
        // Not yet lowered: derive the param types from the AST signature.
        let inst = f.inst.clone();
        if let Some(Item::Fn { params, .. }) = self.lo.fn_items.get(&inst.fn_def) {
            return params
                .iter()
                .map(|p| self.lo.param_type(&inst, p))
                .collect();
        }
        Vec::new()
    }

    /// Resolves a direct/method call to a monomorphized [`FnId`], returning the
    /// receiver argument operands (if method-call sugar) and how many leading
    /// params the receiver consumes.
    fn resolve_direct_call(
        &mut self,
        callee: &Expr,
        _span: Span,
    ) -> Option<(FnId, Vec<Operand>, usize)> {
        match callee {
            Expr::Ident { span, .. } => {
                let def = self.resolved_def(*span)?;
                if !self.lo.fn_items.contains_key(&def) {
                    return None;
                }
                // A bare sibling call inside a generic method (`quick(arr, ...)`
                // from `Sorter(T, Ctx).sort`) must inherit the SAME instantiation
                // key, so the sibling is monomorphized per `(T, Ctx)` and its body
                // resolves `Ctx.method` for THIS instantiation. Otherwise it lowers
                // once (`InstId::plain`) and every instantiation shares one copy —
                // collapsing distinct comptime-type-param dispatch to a single
                // (last-checked) target. Keyed only when `def` is a sibling method
                // of the current instantiated struct.
                let inst = if let Some(struct_ty) = self.current_inst_struct_ty() {
                    if self.is_sibling_method(struct_ty, def) {
                        InstId {
                            fn_def: def,
                            args: vec![InstArgKey::Type(struct_ty)],
                        }
                    } else {
                        InstId::plain(def)
                    }
                } else {
                    InstId::plain(def)
                };
                let fid = self.lo.enqueue(inst);
                Some((fid, Vec::new(), 0))
            }
            Expr::Field { base, span, .. } => {
                // Method-call sugar or an associated call on an instantiated type.
                match self.member_at(*span) {
                    Some(MemberRes::Decl(method_def)) => {
                        // Determine the monomorphization key: the receiver/base
                        // type. For an associated call (`List(u32).init`), the base
                        // span is type-valued and gives the instantiated struct.
                        let bspan = base.span();
                        let inst_arg = self
                            .lo
                            .typed
                            .type_valued_spans
                            .get(&(bspan.start, bspan.end))
                            .copied();
                        if let Some(struct_ty) = inst_arg {
                            // Associated call: no implicit receiver.
                            let inst = InstId {
                                fn_def: method_def,
                                args: vec![InstArgKey::Type(struct_ty)],
                            };
                            let fid = self.lo.enqueue(inst);
                            Some((fid, Vec::new(), 0))
                        } else {
                            // Method call: receiver is the implicit first arg.
                            let recv_ty = self.type_at(bspan);
                            let key_ty = self.peel_ptr(recv_ty);
                            let inst = if self.fn_is_generic_method(method_def) {
                                InstId {
                                    fn_def: method_def,
                                    args: vec![InstArgKey::Type(key_ty)],
                                }
                            } else {
                                InstId::plain(method_def)
                            };
                            let fid = self.lo.enqueue(inst);
                            let recv = self.build_receiver_operand(base, method_def, fid);
                            Some((fid, vec![recv], 1))
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `true` if `method_def` is a member declaration of the instantiated struct
    /// `struct_ty` — i.e. a sibling method reachable by a bare name from inside
    /// another method of the same generic instantiation.
    fn is_sibling_method(&self, struct_ty: TypeId, method_def: DefId) -> bool {
        if let Type::Struct(sid) = self.lo.typed.arena.get(struct_ty) {
            return self.lo.typed.arena.structs[sid.0 as usize]
                .decls
                .iter()
                .any(|d| d.def == method_def);
        }
        false
    }

    /// `true` if a method's defining struct is an instantiated generic (so it
    /// must be keyed by its receiver type for monomorphization).
    fn fn_is_generic_method(&self, method_def: DefId) -> bool {
        // A method defined inside a generic's returned struct is keyed by Self.
        // We detect this by whether the method's own fn item is indexed under a
        // generic; conservatively, any method whose first param is `*Self`/`Self`
        // and whose enclosing type was instantiated. We approximate by checking
        // the method is NOT a plain top-level fn item resolved directly.
        self.lo.fn_items.contains_key(&method_def)
            && self
                .lo
                .resolved
                .defs
                .get(method_def.index())
                .map(|d| matches!(d.kind, k2_resolve::DefKind::Item))
                .unwrap_or(false)
    }

    /// Builds the receiver operand for a method call, auto-`&`-ing to a pointer
    /// receiver when the callee's first param is a pointer.
    ///
    /// The callee may have just been *enqueued* and not yet lowered, so its
    /// lowered `funcs` entry can be absent; we therefore decide `wants_ptr` from
    /// the method's AST signature (its first parameter's type expression, `*Self`
    /// / `*const Self` vs `Self`), which is always available. This is what makes a
    /// `self.field = ...` mutation through a `*Self` receiver actually persist:
    /// the receiver must be passed as the *address* of the base, not a by-value
    /// copy.
    fn build_receiver_operand(&mut self, base: &Expr, method_def: DefId, fid: FnId) -> Operand {
        let recv_ty = self.type_at(base.span());
        let wants_ptr = self
            .method_param0_is_ptr(method_def)
            .or_else(|| {
                // Fallback: the lowered fn (if already present) tells us directly.
                self.lo.funcs.get(fid.index()).and_then(|f| {
                    f.params.first().map(|p| {
                        matches!(
                            self.lo.typed.arena.get(f.locals[p.index()].ty),
                            Type::Pointer { .. }
                        )
                    })
                })
            })
            .unwrap_or(false);
        let param0 = self
            .lo
            .funcs
            .get(fid.index())
            .and_then(|f| f.params.first().map(|p| f.locals[p.index()].ty));
        let base_is_ptr = matches!(self.lo.typed.arena.get(recv_ty), Type::Pointer { .. });
        if wants_ptr && !base_is_ptr {
            // Auto-&: take the address of the receiver place.
            let place = self.lower_place(base);
            self.mark_address_taken(place.base);
            // The receiver pointer type: the callee's first param type if its
            // lowered fn is available, else a pointer to the receiver's own type
            // with the const-ness read from the method's AST signature (so a
            // `*const Self` receiver stays a read-only borrow).
            let is_const = self.method_param0_is_const_ptr(method_def);
            let ptr_ty = param0.unwrap_or_else(|| self.lo.typed.arena.ptr(is_const, recv_ty));
            let tmp = self.new_temp(ptr_ty, base.span());
            self.assign(
                Place::local(tmp),
                Rvalue::Ref {
                    place,
                    is_const: matches!(
                        self.lo.typed.arena.get(ptr_ty),
                        Type::Pointer { is_const: true, .. }
                    ),
                    ty: ptr_ty,
                },
                base.span(),
            );
            Operand::local(tmp)
        } else if !wants_ptr && base_is_ptr {
            // Auto-deref: the method takes Self by value but base is a pointer.
            let place = self.lower_place(base).project(Proj::Deref);
            Operand::Copy(place)
        } else {
            self.lower_operand(base, recv_ty)
        }
    }

    /// `Some(true)` if the method's AST first parameter is a pointer receiver
    /// (`*Self` / `*const Self` / `*T`), `Some(false)` if it is a by-value
    /// receiver, `None` if the method body / signature is unavailable. Read from
    /// the method's own AST so it is valid even before the callee is lowered.
    fn method_param0_is_ptr(&self, method_def: DefId) -> Option<bool> {
        let item = self.lo.fn_items.get(&method_def)?;
        if let Item::Fn { params, .. } = item {
            let p0 = params.first()?;
            return Some(matches!(p0.ty, Expr::Pointer { .. }));
        }
        None
    }

    /// `true` if the method's AST first parameter is a `*const` pointer receiver
    /// (so the auto-`&` borrow is read-only). A by-value or non-const receiver is
    /// `false`.
    fn method_param0_is_const_ptr(&self, method_def: DefId) -> bool {
        if let Some(Item::Fn { params, .. }) = self.lo.fn_items.get(&method_def) {
            if let Some(p0) = params.first() {
                if let Expr::Pointer { is_const, .. } = &p0.ty {
                    return *is_const;
                }
            }
        }
        false
    }

    // ===================================================================
    //  try
    // ===================================================================

    /// Lowers `try operand`: branch on is-error; on error run errdefers and
    /// return the error; on ok yield the success payload. Returns the payload
    /// operand for the ok path (lowering continues in the ok block).
    pub(super) fn lower_try(&mut self, operand: &Expr, span: Span) -> Operand {
        let eu_ty = self.type_at(operand.span());
        let eu = self.lower_operand(operand, eu_ty);
        let is_err = self.new_temp(self.lo.typed.arena.t_bool(), span);
        self.assign(
            Place::local(is_err),
            Rvalue::Discriminant {
                operand: eu.clone(),
                kind: DiscrKind::ErrorUnion,
            },
            span,
        );
        let err_bb = self.new_block();
        let ok_bb = self.new_block();
        self.set_term(Terminator::Branch {
            cond: Operand::local(is_err),
            then_bb: err_bb,
            else_bb: ok_bb,
        });
        // err_bb: run errdefers (error path), build the error return, Return.
        self.cur = err_bb;
        self.run_scope_exit_all(true);
        let ret_ty = self.func.ret;
        let err_tmp = self.new_temp(ret_ty, span);
        // Propagate the original error union as the error return value.
        self.assign(Place::local(err_tmp), Rvalue::Use(eu.clone()), span);
        self.assign(
            Place::local(LocalId(0)),
            Rvalue::Use(Operand::local(err_tmp)),
            span,
        );
        // The error-path return carries the `try` site's span so the VM records
        // an error-return-trace frame as the error propagates out of this fn.
        self.set_term(Terminator::Return {
            value: Operand::local(err_tmp),
            err_trace: Some(span),
        });
        // ok_bb: continue here; the try value is the ok payload.
        self.cur = ok_bb;
        let ok_ty = self.type_at(span);
        self.operand_payload(eu, ok_ty, DiscrKind::ErrorUnion)
    }

    // ===================================================================
    //  catch
    // ===================================================================

    /// Lowers `lhs catch [|err|] rhs` into `dst`.
    fn lower_catch_into(
        &mut self,
        dst: Place,
        lhs: &Expr,
        capture: Option<&str>,
        rhs: &Expr,
        span: Span,
    ) {
        let eu_ty = self.type_at(lhs.span());
        let eu = self.lower_operand(lhs, eu_ty);
        let is_err = self.new_temp(self.lo.typed.arena.t_bool(), span);
        self.assign(
            Place::local(is_err),
            Rvalue::Discriminant {
                operand: eu.clone(),
                kind: DiscrKind::ErrorUnion,
            },
            span,
        );
        let err_bb = self.new_block();
        let ok_bb = self.new_block();
        let join = self.new_block();
        self.set_term(Terminator::Branch {
            cond: Operand::local(is_err),
            then_bb: err_bb,
            else_bb: ok_bb,
        });
        // ok: dst = payload(lhs); goto join.
        self.cur = ok_bb;
        let ok_ty = self.type_at(span);
        let payload = self.operand_payload(eu.clone(), ok_ty, DiscrKind::ErrorUnion);
        self.assign(dst.clone(), Rvalue::Use(payload), span);
        self.set_term(Terminator::Goto(join));
        // err: bind |err| to the error operand; lower rhs (value or diverging).
        self.cur = err_bb;
        if capture.is_some() {
            if let Some(cap_def) = self.catch_capture_def(span, lhs.span()) {
                let err_ty = self.type_at(lhs.span());
                let cap_local = self.declare_local(Some(cap_def), err_ty, span);
                // The capture binds the in-flight error value.
                self.assign(Place::local(cap_local), Rvalue::Use(eu), span);
            }
        }
        self.lower_into(dst, rhs);
        if !self.terminated() {
            self.set_term(Terminator::Goto(join));
        }
        self.cur = join;
    }

    /// Finds the `|err|` capture def of a `catch`, declared between the lhs end
    /// and the catch span.
    fn catch_capture_def(&self, catch_span: Span, lhs_span: Span) -> Option<DefId> {
        self.lo
            .resolved
            .defs
            .iter()
            .find(|d| {
                matches!(d.kind, k2_resolve::DefKind::Capture)
                    && d.span.start >= lhs_span.end
                    && d.span.end <= catch_span.end
            })
            .map(|d| d.id)
    }

    // ===================================================================
    //  unreachable
    // ===================================================================

    /// Lowers an `unreachable` expression: a trap in safe builds, a dead-code
    /// terminator in ReleaseFast.
    fn lower_unreachable(&mut self, span: Span) {
        if self.lo.mode.checks_enabled() {
            self.emit(Statement::Check(SafetyCheck {
                kind: CheckKind::Unreachable,
                span,
            }));
            self.set_term(Terminator::Trap {
                reason: TrapReason::Unreachable,
            });
        } else {
            self.set_term(Terminator::Unreachable);
        }
    }

    // ===================================================================
    //  Builtins
    // ===================================================================

    /// Lowers an `@builtin(args)` call into `dst`.
    fn lower_builtin_into(&mut self, dst: Place, name: &str, args: &[Expr], span: Span) {
        let ty = self.type_at(span);
        match name {
            // `@splat(v)`: broadcast a scalar across every lane of the expected
            // vector. Desugars to an array aggregate of N copies (spec §02).
            "@splat" => {
                if let (Type::Vector { len, elem }, [v]) = (self.lo.typed.arena.get(ty), args) {
                    let len = *len;
                    let elem = *elem;
                    let op = self.lower_operand(v, elem);
                    let src = self.materialize_operand(op, elem, span);
                    let fields = (0..len).map(|_| Operand::local(src)).collect();
                    self.assign(
                        dst,
                        Rvalue::Aggregate {
                            kind: AggKind::Array,
                            fields,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            // `@reduce(.Op, vec)`: fold the lanes with the op into a scalar (spec
            // §02). Desugars to a left fold of per-lane scalar ops.
            "@reduce" => self.lower_reduce_into(dst, args, ty, span),
            "@as" => {
                // @as(T, e): widening cast.
                if let [_, e] = args {
                    let op = self.lower_operand(e, ty);
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::Widen,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            "@intCast" => {
                if let [e] = args {
                    let ety = self.type_at(e.span());
                    let op = self.lower_operand(e, ety);
                    if self.lo.mode.checks_enabled() {
                        self.emit(Statement::Check(SafetyCheck {
                            kind: CheckKind::NarrowFits {
                                value: op.clone(),
                                ty,
                            },
                            span,
                        }));
                    }
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::IntNarrow,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            "@ptrCast" | "@bitCast" | "@ptrFromInt" | "@intFromPtr" => {
                // Pointer/representation reinterpretations: the bits are carried
                // through unchanged.
                if let [e] = args {
                    let ety = self.type_at(e.span());
                    let op = self.lower_operand(e, ety);
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::PtrReinterpret,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            // `@truncate(e)` narrows by *wrapping* to the result width — unlike
            // `@intCast` it is defined to discard high bits, so it gets NO
            // `NarrowFits` check. `@intFromEnum`/`@enumFromInt`/`@floatCast` are
            // likewise width/representation changes the VM's `IntNarrow`/`Widen`
            // path handles (reading an enum tag / re-masking an int / re-rounding
            // a float).
            "@truncate" | "@floatCast" | "@intFromEnum" | "@enumFromInt" => {
                if let [e] = args {
                    let ety = self.type_at(e.span());
                    let op = self.lower_operand(e, ety);
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::IntNarrow,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            // `@intFromFloat(e)`: truncating float -> int.
            "@intFromFloat" => {
                if let [e] = args {
                    let ety = self.type_at(e.span());
                    let op = self.lower_operand(e, ety);
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::FloatToInt,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            // `@floatFromInt(e)`: lossless int -> float.
            "@floatFromInt" => {
                if let [e] = args {
                    let ety = self.type_at(e.span());
                    let op = self.lower_operand(e, ety);
                    self.assign(
                        dst,
                        Rvalue::Cast {
                            kind: CastKind::IntToFloat,
                            operand: op,
                            ty,
                        },
                        span,
                    );
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
            "@panic" => {
                self.set_term(Terminator::Trap {
                    reason: TrapReason::Panic,
                });
            }
            // `@errorReturnTrace()` yields an opaque `?*StackTrace` handle. For
            // v0.20 the *value* is always `null` (the program can null-check it /
            // pass it around); the real product — the error-return trace — is
            // printed automatically by the runtime when an error escapes `main`
            // in Debug/ReleaseSafe (recorded at each `try` site, see the VM's
            // `ReturnErr`). Lowering to a null optional keeps the surface honest
            // while the trace machinery does the work.
            "@errorReturnTrace" => {
                self.assign(dst, Rvalue::MakeNull(ty), span);
            }
            "@errorName" | "@typeName"
            // The std capability/allocator floor: thin `@builtin` leaf intrinsics
            // the VM implements over the managed heap and the *System capabilities.
            // The std allocator structs (`std.heap.*`) and `std.mem.Allocator`'s
            // methods call these, passing a `u32` handle id the VM dispatches on,
            // so different allocator kinds behave differently without fn-pointer
            // vtables. See `crates/k2-vm/src/vm.rs::dispatch_intrinsic`.
            | "@allocId" | "@allocHandle"
            | "@allocRaw" | "@reallocRaw" | "@freeRaw" | "@createRaw" | "@destroyRaw"
            | "@arenaDeinit" | "@gpaDeinit"
            | "@clockNow" | "@clockSleep" | "@randomBytes" | "@randomInt"
            | "@envGet" | "@bufPrint"
            // The concurrency / scheduler floor (v0.11): thin `@builtin` leaf
            // intrinsics the std `Executor`/`Channel`/`Mutex`/`atomic`/`WaitGroup`
            // types call over the VM's deterministic cooperative fiber scheduler.
            // See `crates/k2-vm/src/sched.rs` and the VM dispatcher.
            | "@schedSpawn" | "@schedYield" | "@schedAwait" | "@schedRun"
            | "@chanMake" | "@chanSend" | "@chanRecv" | "@chanClose" | "@chanLen"
            | "@mutexMake" | "@mutexLock" | "@mutexUnlock"
            | "@atomicMake" | "@atomicLoad" | "@atomicStore" | "@atomicFetchAdd"
            | "@atomicSwap" | "@atomicCas"
            | "@wgMake" | "@wgAdd" | "@wgDone" | "@wgWait" => {
                // Runtime ops on opaque data -> intrinsic. `@schedSpawn`'s first
                // argument is the work *function*: lower it to an `FnRef` const
                // tag (the MIR has no indirect calls), so the VM can build the new
                // fiber's root frame from that callee.
                let mut arg_ops = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    if name == "@schedSpawn" && i == 0 {
                        arg_ops.push(self.lower_fn_ref_arg(a));
                        continue;
                    }
                    let aty = self.type_at(a.span());
                    arg_ops.push(self.lower_call_arg(a, aty));
                }
                self.assign(
                    dst,
                    Rvalue::Intrinsic {
                        path: IntrinsicPath {
                            root: IntrinsicRoot::Builtin(name.trim_start_matches('@').to_string()),
                            members: Vec::new(),
                            is_call: true,
                        },
                        args: arg_ops,
                        ty,
                    },
                    span,
                );
            }
            _ => {
                // A comptime-folded builtin (`@sizeOf`, `@typeInfo`, `@This`,
                // `@import`, `@field` on a comptime name, …): inline the folded
                // value if known, else undef.
                if let Some(c) = self.comptime_span_const(span, ty) {
                    self.assign(dst, Rvalue::Use(Operand::Const(c)), span);
                } else {
                    self.assign(dst, Rvalue::Use(Operand::Const(Const::Undef { ty })), span);
                }
            }
        }
    }

    // ===================================================================
    //  Initializers
    // ===================================================================

    /// Lowers a struct/array/tuple initializer into `dst`.
    fn lower_init_into(
        &mut self,
        dst: Place,
        _ty: Option<&Expr>,
        body: &k2_syntax::InitBody,
        span: Span,
    ) {
        let agg_ty = self.type_at(span);
        match body {
            k2_syntax::InitBody::Fields(fields) => {
                // `.{}` empty initializer on a slice type -> the empty slice.
                if fields.is_empty()
                    && matches!(self.lo.typed.arena.get(agg_ty), Type::Slice { .. })
                {
                    self.assign(
                        dst,
                        Rvalue::Use(Operand::Const(Const::EmptySlice { ty: agg_ty })),
                        span,
                    );
                    return;
                }
                // Build the field operands in layout order. We map each `.name`
                // to its struct field index when the type is a struct.
                let ordered = self.order_struct_fields(agg_ty, fields);
                self.assign(
                    dst,
                    Rvalue::Aggregate {
                        kind: AggKind::Struct,
                        fields: ordered,
                        ty: agg_ty,
                    },
                    span,
                );
            }
            k2_syntax::InitBody::Tuple(elems) => {
                // A `@Vector(N,T)` literal lowers like an array aggregate (the VM
                // represents both as `Value::Array`; native lays both out
                // contiguously), so a vector init reuses the array path.
                let is_array = matches!(
                    self.lo.typed.arena.get(agg_ty),
                    Type::Array { .. } | Type::Vector { .. }
                );
                let elem_ty = self.array_elem(agg_ty);
                let mut ops = Vec::with_capacity(elems.len());
                for e in elems {
                    let t = if is_array {
                        elem_ty
                    } else {
                        self.type_at(e.span())
                    };
                    ops.push(self.lower_operand(e, t));
                }
                self.assign(
                    dst,
                    Rvalue::Aggregate {
                        kind: if is_array {
                            AggKind::Array
                        } else {
                            AggKind::Tuple
                        },
                        fields: ops,
                        ty: agg_ty,
                    },
                    span,
                );
            }
        }
    }

    /// The element type of an array/slice/vector type (best effort).
    pub(super) fn array_elem(&self, ty: TypeId) -> TypeId {
        match self.lo.typed.arena.get(ty) {
            Type::Array { elem, .. } | Type::Slice { elem, .. } | Type::Vector { elem, .. } => {
                *elem
            }
            _ => ty,
        }
    }

    /// Orders named field initializers into struct layout order, lowering each
    /// to an operand of the field's type.
    fn order_struct_fields(
        &mut self,
        agg_ty: TypeId,
        fields: &[k2_syntax::FieldInit],
    ) -> Vec<Operand> {
        // Gather the struct's declared field names + types, if a struct.
        let layout: Vec<(String, TypeId)> = match self.lo.typed.arena.get(agg_ty).clone() {
            Type::Struct(sid) => self.lo.typed.arena.structs[sid.0 as usize]
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.ty))
                .collect(),
            _ => Vec::new(),
        };
        if layout.is_empty() {
            // Anonymous / unknown: keep source order.
            return fields
                .iter()
                .map(|fi| {
                    let t = self.type_at(fi.value.span());
                    self.lower_operand(&fi.value, t)
                })
                .collect();
        }
        let mut ops = Vec::with_capacity(layout.len());
        for (fname, fty) in &layout {
            if let Some(fi) = fields.iter().find(|fi| &fi.name == fname) {
                ops.push(self.lower_operand(&fi.value, *fty));
            } else {
                // Defaulted/omitted field: undef placeholder (default applied by VM).
                ops.push(Operand::Const(Const::Undef { ty: *fty }));
            }
        }
        ops
    }

    // ===================================================================
    //  Constants & folded values
    // ===================================================================

    /// If `e` is a bare identifier resolving to a top-level (file-scope) *value*
    /// const with a recorded initializer, returns a clone of that initializer.
    /// Such a const has no runtime local slot, so a bare reference to it must
    /// inline the initializer to materialize the value (mirroring the namespaced
    /// member-access inlining in [`Self::lower_field_into`]). A reference that
    /// already has a local slot is handled by [`Self::try_lower_place`] and never
    /// reaches here.
    pub(super) fn top_level_const_init(&self, e: &Expr) -> Option<Expr> {
        let Expr::Ident { span, .. } = e else {
            return None;
        };
        let def = self.resolved_def(*span)?;
        // Only inline when there is genuinely no local slot for the binding (a
        // shadowing local would have been caught by `try_lower_place`).
        if self.locals_by_def.contains_key(&def) {
            return None;
        }
        self.lo.value_const_inits.get(&def).cloned()
    }

    /// Builds an [`Const`] for a literal/enum/error expression, if possible.
    pub(super) fn const_of(&mut self, e: &Expr, expected: TypeId) -> Option<Const> {
        let ty = if self.lo.typed.arena.is_bottom(expected) {
            self.type_at(e.span())
        } else {
            expected
        };
        match e {
            Expr::Int { text, base, .. } => {
                let value = parse_int(text, *base)?;
                let cty = if self.is_sized_int(ty) || self.is_comptime_int(ty) {
                    ty
                } else {
                    self.lo.typed.arena.t_comptime_int()
                };
                Some(Const::Int { value, ty: cty })
            }
            Expr::Float { text, .. } => {
                let v: f64 = text.replace('_', "").parse().ok()?;
                Some(Const::Float {
                    bits: v.to_bits(),
                    ty,
                })
            }
            Expr::Bool { value, .. } => Some(Const::Bool(*value)),
            Expr::Null { .. } => Some(Const::Undef { ty }), // optional null built via MakeNull elsewhere
            Expr::Undefined { .. } => Some(Const::Undef { ty }),
            Expr::Char { text, .. } => {
                let v = parse_char(text)?;
                Some(Const::Int {
                    value: v as i128,
                    ty,
                })
            }
            Expr::Str { text, .. } => {
                let bytes = decode_str(text);
                let id = self.lo.intern_str(bytes);
                Some(Const::Str(id))
            }
            Expr::ErrorLiteral { name, .. } => {
                let tag = self.lo.err_tag(name);
                Some(Const::ErrVal { tag, ty })
            }
            Expr::EnumLiteral { span, .. } => {
                if let Some(MemberRes::Variant(idx)) = self.member_at(*span) {
                    Some(Const::EnumVal { variant: idx, ty })
                } else {
                    Some(Const::Undef { ty })
                }
            }
            _ => None,
        }
    }

    /// `true` if `ty` is `comptime_int`.
    fn is_comptime_int(&self, ty: TypeId) -> bool {
        matches!(self.lo.typed.arena.get(ty), Type::ComptimeInt)
    }

    /// The comptime-folded constant at `span`, if the checker recorded one.
    pub(super) fn comptime_span_const(&self, span: Span, ty: TypeId) -> Option<Const> {
        if let Some(&v) = self
            .lo
            .typed
            .comptime_span_ints
            .get(&(span.start, span.end))
        {
            let cty = if self.is_sized_int(ty) || self.is_comptime_int(ty) {
                ty
            } else {
                self.lo.typed.arena.t_usize()
            };
            return Some(Const::Int { value: v, ty: cty });
        }
        None
    }

    /// A folded constant for a `const`/`var` binding's initializer, if the value
    /// is comptime-known (so no runtime code is emitted for it).
    pub(super) fn folded_const(&self, def: Option<DefId>, value: &Expr) -> Option<Const> {
        let ty = self.type_at(value.span());
        // A binding folded to a comptime int.
        if let Some(d) = def {
            if let Some(&v) = self.lo.typed.comptime_int_values.get(&d) {
                let cty = if self.is_sized_int(ty) {
                    ty
                } else {
                    self.lo.typed.arena.t_comptime_int()
                };
                return Some(Const::Int { value: v, ty: cty });
            }
        }
        // A `comptime <expr>` initializer folded at the value span.
        if let Some(&v) = self
            .lo
            .typed
            .comptime_span_ints
            .get(&(value.span().start, value.span().end))
        {
            let cty = if self.is_sized_int(ty) {
                ty
            } else {
                self.lo.typed.arena.t_usize()
            };
            return Some(Const::Int { value: v, ty: cty });
        }
        None
    }

    // ===================================================================
    //  Intrinsic path building
    // ===================================================================

    /// If `callee(args)` targets a still-Deferred std/sys/build member, returns
    /// the intrinsic path plus any receiver operand(s) to prepend.
    fn try_intrinsic_call(
        &mut self,
        callee: &Expr,
        _args: &[Expr],
    ) -> Option<(IntrinsicPath, Vec<Operand>)> {
        let Expr::Field { base, field, span } = callee else {
            return None;
        };
        // Only treat as intrinsic when the member is Deferred (std/sys/build) OR
        // the base type is a module/opaque/deferred namespace.
        let is_deferred_member = matches!(self.member_at(*span), Some(MemberRes::Deferred) | None);
        let base_ty = self.type_at(base.span());
        let base_is_opaque = matches!(
            self.lo.typed.arena.get(self.peel_ptr(base_ty)),
            Type::Module(_) | Type::Opaque(_) | Type::Deferred | Type::AnyType
        );
        if !(is_deferred_member && (base_is_opaque || self.base_chain_is_deferred(base))) {
            // Not an intrinsic: a concrete method/field call handled elsewhere.
            // But if the member did NOT resolve to a Decl/Field/Variant, it's the
            // std/sys boundary regardless of the base, so still intrinsic.
            if !is_deferred_member {
                return None;
            }
        }
        let path = self.build_intrinsic_path(base, std::slice::from_ref(field), true)?;
        Some((path, Vec::new()))
    }

    /// `true` if a base expression chain bottoms out in a Deferred/opaque value
    /// (so a member call on it is an intrinsic).
    fn base_chain_is_deferred(&self, base: &Expr) -> bool {
        let bty = self.type_at(base.span());
        if matches!(
            self.lo.typed.arena.get(self.peel_ptr(bty)),
            Type::Module(_) | Type::Opaque(_) | Type::Deferred | Type::AnyType
        ) {
            return true;
        }
        match base {
            Expr::Field { base, span, .. } => {
                matches!(self.member_at(*span), Some(MemberRes::Deferred) | None)
                    || self.base_chain_is_deferred(base)
            }
            Expr::Call { callee, .. } => self.base_chain_is_deferred(callee),
            Expr::Ident { span, .. } => {
                // An ident whose type is Deferred/opaque (e.g. `out: anytype`).
                let t = self.type_at(*span);
                matches!(
                    self.lo.typed.arena.get(self.peel_ptr(t)),
                    Type::Deferred | Type::AnyType | Type::Opaque(_) | Type::Module(_)
                )
            }
            _ => false,
        }
    }

    /// Builds an [`IntrinsicPath`] for a member chain `base.<trailing...>`.
    /// Walks the `Field`/`Call`/`Ident` chain to find the root and members.
    pub(super) fn build_intrinsic_path(
        &mut self,
        base: &Expr,
        trailing: &[String],
        is_call: bool,
    ) -> Option<IntrinsicPath> {
        let mut members: Vec<String> = Vec::new();
        let root = self.walk_intrinsic_chain(base, &mut members)?;
        members.extend(trailing.iter().cloned());
        Some(IntrinsicPath {
            root,
            members,
            is_call,
        })
    }

    /// Builds an intrinsic path directly from a call's callee chain.
    fn build_intrinsic_path_from_call(&mut self, callee: &Expr) -> Option<IntrinsicPath> {
        match callee {
            Expr::Field { base, field, .. } => {
                self.build_intrinsic_path(base, std::slice::from_ref(field), true)
            }
            Expr::Ident { name, span } => {
                // A bare fn-name call we could not resolve: treat the name as a
                // single-member intrinsic rooted at a builtin namespace.
                let _ = span;
                Some(IntrinsicPath {
                    root: IntrinsicRoot::Builtin("call".to_string()),
                    members: vec![name.clone()],
                    is_call: true,
                })
            }
            _ => None,
        }
    }

    /// Walks an intrinsic base chain, collecting member names (innermost-first
    /// reversed to source order) and returning the chain root.
    fn walk_intrinsic_chain(
        &mut self,
        e: &Expr,
        members: &mut Vec<String>,
    ) -> Option<IntrinsicRoot> {
        match e {
            Expr::Ident { span, name } => {
                match self.lo.resolved.uses.at(*span).map(|u| u.res) {
                    Some(Resolution::Module(def)) => Some(IntrinsicRoot::Module(def)),
                    _ => {
                        // A value whose type is Deferred/opaque (sys, b, out, …):
                        // the root is the value operand.
                        let ty = self.type_at(*span);
                        if let Some(def) = self.resolved_def(*span) {
                            if let Some(&local) = self.locals_by_def.get(&def) {
                                return Some(IntrinsicRoot::Value(Box::new(Operand::local(local))));
                            }
                        }
                        let _ = (ty, name);
                        // Unknown ident root: a builtin namespace fallback.
                        Some(IntrinsicRoot::Builtin(name.clone()))
                    }
                }
            }
            Expr::Field { base, field, span } => {
                // If this member resolves concretely (a real value), the root is
                // this value; otherwise keep climbing collecting names.
                let resolved_value = matches!(
                    self.member_at(*span),
                    Some(MemberRes::Field(_))
                        | Some(MemberRes::PackedField(..))
                        | Some(MemberRes::BuiltinField)
                );
                if resolved_value {
                    // A concrete sub-value: materialize it as the root operand.
                    let op = self.lower_operand(e, self.type_at(*span));
                    return Some(IntrinsicRoot::Value(Box::new(op)));
                }
                let root = self.walk_intrinsic_chain(base, members)?;
                members.push(field.clone());
                Some(root)
            }
            Expr::Call { callee, .. } => {
                // A call in the chain (e.g. `gpa.allocator()`): its result is the
                // root value, materialized via an intrinsic call operand.
                let ty = self.type_at(e.span());
                let tmp = self.new_temp(ty, e.span());
                // Lower the call (it is itself likely an intrinsic).
                let dst = Place::local(tmp);
                self.lower_call_into(dst, callee, &call_args(e), e.span());
                Some(IntrinsicRoot::Value(Box::new(Operand::local(tmp))))
            }
            _ => {
                let ty = self.type_at(e.span());
                let op = self.lower_operand(e, ty);
                Some(IntrinsicRoot::Value(Box::new(op)))
            }
        }
    }

    // ===================================================================
    //  Leak-pass support
    // ===================================================================

    /// Records, for the leak pass, whether a binding's initializer is an
    /// allocating intrinsic (so a missing free can be flagged).
    pub(super) fn record_alloc_binding(&mut self, _local: LocalId, _value: &Expr) {
        // Allocation detection is done structurally by the leak pass over the
        // lowered MIR (it scans `Intrinsic` member names), so nothing to record
        // here beyond what the MIR already encodes.
    }

    /// Notes that a `defer`/`errdefer` body releases a local (frees a resource),
    /// so the leak pass treats that local's ownership as handled.
    pub(super) fn note_release_from_defer(&mut self, body: &Stmt) {
        // Find a free/destroy/deinit call inside the defer body and record the
        // local it releases.
        let mut released = Vec::new();
        collect_released_locals(self, body, &mut released);
        for l in released {
            if !self.alloc_info.released.contains(&l) {
                self.alloc_info.released.push(l);
            }
        }
    }
}

/// Collects the locals released by a defer/errdefer body (the first arg of a
/// `free`/`destroy`/`deinit` intrinsic call, or the receiver of `x.deinit()`).
fn collect_released_locals(fb: &FnBuilder<'_, '_>, stmt: &Stmt, out: &mut Vec<LocalId>) {
    match stmt {
        Stmt::Expr { expr, .. } => collect_released_in_expr(fb, expr, out),
        Stmt::Block { body, .. } => {
            for s in body {
                collect_released_locals(fb, s, out);
            }
        }
        _ => {}
    }
}

/// Helper: scans an expression for a release call and records the freed local.
fn collect_released_in_expr(fb: &FnBuilder<'_, '_>, e: &Expr, out: &mut Vec<LocalId>) {
    if let Expr::Call { callee, args, .. } = e {
        if let Expr::Field { base, field, .. } = &**callee {
            if matches!(field.as_str(), "free" | "destroy" | "deinit") {
                // `x.deinit()` releases `x`; `alloc.free(x)` releases the arg.
                if field == "deinit" {
                    record_ident_local(fb, base, out);
                } else if let Some(a) = args.first() {
                    record_ident_local(fb, a, out);
                }
            }
        }
    }
}

/// Records the local an identifier refers to, if it names a binding.
fn record_ident_local(fb: &FnBuilder<'_, '_>, e: &Expr, out: &mut Vec<LocalId>) {
    if let Expr::Ident { span, .. } = e {
        if let Some(def) = fb.resolved_def(*span) {
            if let Some(&local) = fb.locals_by_def.get(&def) {
                if !out.contains(&local) {
                    out.push(local);
                }
            }
        }
    }
}

/// Extracts the args of a call expression.
fn call_args(e: &Expr) -> Vec<Expr> {
    if let Expr::Call { args, .. } = e {
        args.clone()
    } else {
        Vec::new()
    }
}

/// `true` if `e` is an empty initializer `.{}` (no fields, no tuple elements).
fn is_empty_init(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Init {
            body: k2_syntax::InitBody::Fields(f),
            ..
        } if f.is_empty()
    ) || matches!(
        e,
        Expr::Init {
            body: k2_syntax::InitBody::Tuple(t),
            ..
        } if t.is_empty()
    )
}

/// Maps an AST binary operator to a MIR binary operator (the non-control ones).
fn map_binop(op: AstBinOp) -> BinOp {
    match op {
        AstBinOp::Add => BinOp::Add,
        AstBinOp::Sub => BinOp::Sub,
        AstBinOp::Mul => BinOp::Mul,
        AstBinOp::Div => BinOp::Div,
        AstBinOp::Rem => BinOp::Rem,
        AstBinOp::BitAnd => BinOp::BitAnd,
        AstBinOp::BitOr => BinOp::BitOr,
        AstBinOp::BitXor => BinOp::BitXor,
        AstBinOp::Shl => BinOp::Shl,
        AstBinOp::Shr => BinOp::Shr,
        AstBinOp::Eq => BinOp::Eq,
        AstBinOp::Ne => BinOp::Ne,
        AstBinOp::Lt => BinOp::Lt,
        AstBinOp::Le => BinOp::Le,
        AstBinOp::Gt => BinOp::Gt,
        AstBinOp::Ge => BinOp::Ge,
        // Control-flow ops are handled before this is reached.
        AstBinOp::And
        | AstBinOp::Or
        | AstBinOp::Orelse
        | AstBinOp::Concat
        | AstBinOp::ErrSetMerge => BinOp::Add,
    }
}

/// Parses an integer literal's text (with its radix) into an `i128`.
pub(super) fn parse_int(text: &str, base: k2_syntax::IntBase) -> Option<i128> {
    let clean: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = match base {
        k2_syntax::IntBase::Dec => (10, clean.as_str()),
        k2_syntax::IntBase::Hex => (16, clean.strip_prefix("0x").unwrap_or(&clean)),
        k2_syntax::IntBase::Oct => (8, clean.strip_prefix("0o").unwrap_or(&clean)),
        k2_syntax::IntBase::Bin => (2, clean.strip_prefix("0b").unwrap_or(&clean)),
    };
    i128::from_str_radix(digits, radix).ok()
}

/// Parses a character literal `'a'` into its scalar value.
pub(super) fn parse_char(text: &str) -> Option<u32> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first == '\\' {
        let esc = chars.next()?;
        let v = match esc {
            'n' => b'\n' as u32,
            't' => b'\t' as u32,
            'r' => b'\r' as u32,
            '0' => 0,
            '\\' => b'\\' as u32,
            '\'' => b'\'' as u32,
            '"' => b'"' as u32,
            _ => esc as u32,
        };
        Some(v)
    } else {
        Some(first as u32)
    }
}

/// Decodes a string literal's bytes (stripping the surrounding quotes and
/// interpreting simple escapes). Best effort — the VM never needs exact bytes
/// for the corpus, only a stable, distinct constant.
fn decode_str(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('r') => out.push(b'\r'),
                Some('0') => out.push(0),
                Some('\\') => out.push(b'\\'),
                Some('"') => out.push(b'"'),
                Some('\'') => out.push(b'\''),
                Some(other) => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => {}
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}
