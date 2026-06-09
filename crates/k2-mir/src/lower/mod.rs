//! Lowering: AST -> MIR.
//!
//! The [`Lowerer`] owns the program-level state — the type arena (moved out of
//! [`Typed`]), the monomorphization worklist, the instantiation -> [`FnId`] map,
//! and a stable error-tag interner — and drives a worklist that lowers one
//! [`MirFunction`] per reached `(fn, comptime-arg)` instantiation. The
//! [`FnBuilder`] owns per-function state — the function under construction, the
//! current block, the local table keyed by [`DefId`], and a *scope stack* whose
//! frames hold the registered `defer`/`errdefer` actions and the loop/labeled-
//! block control targets.
//!
//! Every desugaring lives here: `defer`/`errdefer` (LIFO, error-path-only
//! `errdefer`), `try`, `catch`, `orelse`, optional `.?`, error-union and optional
//! construction, `if`/`while`/`for`/`switch`, labeled `break`/`continue` and
//! break-with-value, short-circuit `and`/`or`, method-call receiver sugar, and
//! comptime-folded-value inlining. Still-`Deferred` std/sys/build member calls
//! become opaque [`Rvalue::Intrinsic`] nodes.
//!
//! Safety-check *insertion* is interleaved here (gated by [`BuildMode`]); the
//! mechanical block-splitting that turns a [`Statement::Check`] into a branch to
//! the shared panic block runs as a post-pass in [`crate::checks`].

use std::collections::HashMap;

use k2_resolve::{DefId, Resolution, Resolved};
use k2_syntax::{
    AssignOp, BinOp as AstBinOp, Capture, CaptureName, Expr, ForOperand, Item, Member, SourceFile,
    Span, Stmt, SwitchPattern, UnOp as AstUnOp,
};
use k2_types::{MemberRes, Type, TypeId, Typed};

use crate::checks;
use crate::ir::*;

/// Lowers a fully type-checked file to MIR under `mode`.
pub(crate) fn lower_program(
    file: &SourceFile,
    resolved: &Resolved,
    typed: Typed,
    mode: BuildMode,
) -> Result<MirProgram, Vec<Diagnostic>> {
    let mut lowerer = Lowerer::new(file, resolved, typed, mode);
    lowerer.run();
    // Pull the per-fn alloc facts out before `finish` consumes the lowerer.
    let fn_alloc_info = std::mem::take(&mut lowerer.fn_alloc_info);
    let mut prog = lowerer.finish();
    // Run the conservative leak/escape analysis and merge its findings.
    let leaks = crate::leak::analyze(&prog, &fn_alloc_info);
    prog.diagnostics.extend(leaks);
    Ok(prog)
}

/// The program-level lowering state.
pub(crate) struct Lowerer<'a> {
    /// The source file (for indexing fn items by DefId).
    file: &'a SourceFile,
    /// The resolved side-table (name resolution).
    resolved: &'a Resolved,
    /// The typed result (types per span, members, binding types, folded consts).
    /// Owns the type arena, which the finished program moves out and ships.
    typed: Typed,
    /// The build mode (drives safety-check insertion).
    mode: BuildMode,
    /// Every `fn` item (top-level and nested-in-a-generic) by its DefId.
    fn_items: HashMap<DefId, Item>,
    /// Value-producing `const` decls by DefId, mapping to their initializer
    /// expression. A *value* const is one whose initializer is a runtime value
    /// (e.g. `pub const allocator: Allocator = @allocHandle(@allocId(5, 0));`),
    /// NOT a container/type/error-set/fn. Referencing such a member
    /// (`std.testing.allocator`) inlines its initializer here instead of emitting
    /// an unresolvable `@std.<member>` intrinsic. See
    /// [`FnBuilder::lower_field_into`].
    value_const_inits: HashMap<DefId, Expr>,
    /// Struct field DEFAULT initializers, keyed by `(struct defining span)` ->
    /// `[(field name, default expr)]`. A `struct { x: i64 = 100, y: i64 }` records
    /// `x -> 100`; an initializer `.{ .y = 5 }` that omits `x` lowers this default
    /// into the field instead of leaving it `undef`. The defining span is the
    /// struct's nominal identity key (`StructInfo::span`), so a construction can
    /// look its defaults up from the construction's struct type.
    struct_field_defaults: HashMap<(u32, u32), Vec<(String, Expr)>>,
    /// The monomorphization worklist: instantiations still to lower.
    worklist: Vec<InstId>,
    /// Instantiation -> its assigned FnId (dedup; also lets recursion resolve).
    by_inst: HashMap<InstId, FnId>,
    /// The lowered functions, in id order.
    funcs: Vec<MirFunction>,
    /// The entry points (main + tests) for this build.
    entries: Vec<FnId>,
    /// Interned constant data (string bytes, aggregates).
    consts: Vec<ConstData>,
    /// Interned string-literal bytes -> ConstId (dedup identical literals).
    str_intern: HashMap<Vec<u8>, ConstId>,
    /// Global error name -> stable nonzero tag.
    err_tags: HashMap<String, ErrTag>,
    /// Top-level error-set const DefId -> its member names. Lets the lowerer
    /// recognize `ParseError.NotANumber` (a member access on a named error-set
    /// type, which the checker leaves `Deferred`) as an error *value*.
    err_set_consts: HashMap<DefId, Vec<String>>,
    /// Lowering diagnostics.
    diagnostics: Vec<Diagnostic>,
    /// Per-fn allocation/free facts the leak pass consumes (keyed by FnId).
    pub(crate) fn_alloc_info: HashMap<FnId, FnAllocInfo>,
    /// A guard against runaway instantiation.
    inst_budget: usize,
}

/// The allocation/free facts a single function exposes to the leak pass.
#[derive(Default, Clone)]
pub(crate) struct FnAllocInfo {
    /// Locals released by some `defer`/`errdefer`/explicit free in this fn.
    pub released: Vec<LocalId>,
    /// `true` if this fn's body had any loop (a nested allocation may free
    /// across iterations, so the missing-free heuristic bails).
    pub has_loop: bool,
}

impl<'a> Lowerer<'a> {
    /// Builds a lowerer, indexing every `fn` item by DefId.
    fn new(file: &'a SourceFile, resolved: &'a Resolved, typed: Typed, mode: BuildMode) -> Self {
        let mut fn_items = HashMap::new();
        for item in &file.items {
            index_fn_items(resolved, item, &mut fn_items);
        }
        // Index named error-set consts (`const E = error{...}`) by DefId.
        let mut err_set_consts = HashMap::new();
        for item in &file.items {
            if let Item::Const {
                value: Expr::ErrorSet { fields, .. },
                span,
                ..
            } = item
            {
                if let Some(d) = resolved.defs.iter().find(|d| d.span == *span) {
                    err_set_consts.insert(d.id, fields.clone());
                }
            }
        }
        // Index value-producing `const` decls (incl. those nested in container
        // types like `std.testing`) by DefId, so a member read that resolves to
        // such a const can inline its initializer.
        let mut value_const_inits = HashMap::new();
        for item in &file.items {
            index_value_consts(resolved, item, &mut value_const_inits);
        }
        // Index struct field defaults (incl. structs nested in container types) so
        // an initializer that omits a defaulted field can fill it in.
        let mut struct_field_defaults = HashMap::new();
        for item in &file.items {
            index_struct_field_defaults(item, &mut struct_field_defaults);
        }
        Lowerer {
            file,
            resolved,
            typed,
            mode,
            fn_items,
            value_const_inits,
            struct_field_defaults,
            err_set_consts,
            worklist: Vec::new(),
            by_inst: HashMap::new(),
            funcs: Vec::new(),
            entries: Vec::new(),
            consts: Vec::new(),
            str_intern: HashMap::new(),
            // Pre-seed the std allocator-floor error names so they always carry a
            // stable, nonzero tag even when no source spells them as `error.*`
            // literals: the VM's bounded allocators (fixed-buffer / `@bufPrint`)
            // synthesize these `error.OutOfMemory` / `error.NoSpaceLeft` values
            // directly, and `@errorName`/`catch` must be able to name them.
            err_tags: {
                let mut m = HashMap::new();
                m.insert("OutOfMemory".to_string(), ErrTag(1));
                m.insert("NoSpaceLeft".to_string(), ErrTag(2));
                // Pre-seed the v0.23 fs/net error names for the same reason: the
                // `sys.fs`/`sys.net` capability door (a deferred-member intrinsic)
                // synthesizes these `error.*` values in the VM directly from the
                // host's `io::ErrorKind`, with no `error.*` literal in the source to
                // register them — yet `@errorName`/`catch`/`switch` must still name
                // them. The std `FsError`/`NetError` sets declare exactly these.
                for (i, name) in [
                    "FileNotFound",
                    "AccessDenied",
                    "AlreadyExists",
                    "NotADirectory",
                    "IsADirectory",
                    "IoError",
                    "ConnectionRefused",
                    "ConnectionReset",
                    "AddressInUse",
                    "WouldBlock",
                ]
                .iter()
                .enumerate()
                {
                    m.insert(name.to_string(), ErrTag((i as u16) + 3));
                }
                m
            },
            diagnostics: Vec::new(),
            fn_alloc_info: HashMap::new(),
            inst_budget: 100_000,
        }
    }

    /// Seeds the worklist with entries and drains it. The entry set is `main`,
    /// every `test`, and — so a library file (no `main`) still lowers and tooling
    /// can inspect any function — every top-level NON-generic `fn` with a body. A
    /// generic function (a `comptime`/`anytype` parameter) is never a standalone
    /// entry; it is reached and monomorphized through its call sites.
    fn run(&mut self) {
        for item in &self.file.items {
            match item {
                Item::Fn {
                    name, span, body, ..
                } => {
                    if body.is_none() {
                        continue; // extern/proto: nothing to lower.
                    }
                    let Some(def) = self.def_of(*span) else {
                        continue;
                    };
                    if name == "main" || !self.fn_is_generic(def) {
                        let id = self.enqueue(InstId::plain(def));
                        self.entries.push(id);
                    }
                }
                Item::Test { .. } => {
                    // A `test` block has no DefId of its own; synthesize a fn.
                    let id = self.lower_test(item);
                    self.entries.push(id);
                }
                _ => {}
            }
        }
        // Drain the worklist.
        while let Some(inst) = self.worklist.pop() {
            if self.inst_budget == 0 {
                self.diagnostics.push(Diagnostic::error(
                    Span::default(),
                    "monomorphization budget exhausted",
                ));
                break;
            }
            self.inst_budget -= 1;
            let fid = self.by_inst[&inst];
            // Skip if already lowered (the slot is a real fn, not a placeholder).
            if self.funcs[fid.index()].blocks.len() > 1
                || !self.funcs[fid.index()].blocks.is_empty()
                    && !matches!(
                        self.funcs[fid.index()].blocks[0].term,
                        Terminator::Unreachable
                    )
            {
                continue;
            }
            self.lower_inst(inst, fid);
        }
    }

    /// Allocates (or reuses) a FnId for `inst`, enqueueing it if fresh.
    fn enqueue(&mut self, inst: InstId) -> FnId {
        if let Some(&id) = self.by_inst.get(&inst) {
            return id;
        }
        let id = FnId(self.funcs.len() as u32);
        // Reserve a placeholder fn with an empty entry block; lowering fills it.
        let name = self.inst_display_name(&inst);
        let ret = self.fn_ret_type(&inst);
        // v0.19: resolve the function's C linkage from the typed extern/export
        // table. A plain (non-`extern`/`export`) instantiation stays Internal/K2.
        let (abi, linkage, is_extern_decl, varargs) = self.fn_linkage_of(inst.fn_def);
        let mut f = MirFunction {
            id,
            name,
            def: Some(inst.fn_def),
            abi,
            linkage,
            is_extern_decl,
            varargs,
            inst: inst.clone(),
            params: Vec::new(),
            ret,
            locals: Vec::new(),
            blocks: Vec::new(),
            entry: BlockId(0),
            panic_block: None,
            bool_ty: None,
            span: Span::default(),
        };
        f.new_block(); // placeholder entry
        self.funcs.push(f);
        self.by_inst.insert(inst.clone(), id);
        self.worklist.push(inst);
        id
    }

    /// Finalizes the program (runs the check-splitting post-pass on each fn) and
    /// moves the type arena out of `typed` into the shipped program.
    fn finish(self) -> MirProgram {
        // Build the tag -> name reverse map so the VM can implement `@errorName`
        // and print an error that escapes `main`.
        let err_names: HashMap<ErrTag, String> = self
            .err_tags
            .iter()
            .map(|(name, &tag)| (tag, name.clone()))
            .collect();
        let mut funcs = self.funcs;
        for f in &mut funcs {
            // Drop unreachable join blocks left by lowering (e.g. a labeled block
            // whose every exit is a `break :blk`) BEFORE the check-splitter runs,
            // so the dense block numbering and the new panic block stay coherent.
            f.gc_unreachable_blocks();
            checks::split_checks(f);
        }
        MirProgram {
            arena: self.typed.arena,
            funcs,
            by_inst: self.by_inst,
            entries: self.entries,
            consts: self.consts,
            diagnostics: self.diagnostics,
            mode: self.mode,
            err_names,
        }
    }

    // -------------------------------------------------------------------
    //  Instantiation lowering
    // -------------------------------------------------------------------

    /// Lowers the body of `inst` into the reserved function `fid`.
    fn lower_inst(&mut self, inst: InstId, fid: FnId) {
        let Some(item) = self.fn_items.get(&inst.fn_def).cloned() else {
            // No body available (extern/proto or a method on an instantiated
            // type we cannot reach the AST for): leave a trivial stub.
            self.stub_function(fid);
            return;
        };
        let Item::Fn {
            params,
            ret: _,
            body,
            span,
            ..
        } = &item
        else {
            self.stub_function(fid);
            return;
        };
        let Some(body) = body else {
            // A body-less declaration. For a v0.19 `extern` C function, build a
            // params-only stub so the codegen can read the declared parameter types
            // (it needs them to marshal a string literal as a `const char *` vs a
            // fat slice, and to classify each arg). Other body-less protos stay a
            // trivial stub.
            if self.funcs[fid.index()].is_extern_decl {
                self.extern_stub_function(fid, &inst, params, *span);
            } else {
                self.stub_function(fid);
            }
            return;
        };

        let ret_ty = self.fn_ret_type(&inst);
        let mut fb = FnBuilder::new(self, fid, ret_ty, *span);
        // Bind parameters as locals (slot order = declaration order, after the
        // return slot at index 0).
        for p in params {
            let pty = fb.lo.param_type(&inst, p);
            let def = fb.lo.def_of(p.span);
            fb.add_param(def, pty, p.span, &p.name);
        }
        fb.push_scope(None);
        fb.lower_block_stmts(body);
        // Fall-through off the body: run defers (success path) and return void.
        if !fb.terminated() {
            fb.run_scope_exit_all(false);
            let v = Operand::Const(Const::Void);
            fb.set_term(Terminator::Return {
                value: v,
                err_trace: None,
            });
        }
        fb.pop_scope_no_defers();
        let alloc_info = fb.alloc_info.clone();
        let func = fb.finish();
        self.funcs[fid.index()] = func;
        self.fn_alloc_info.insert(fid, alloc_info);
    }

    /// Lowers a `test` block into a synthesized void function.
    fn lower_test(&mut self, item: &Item) -> FnId {
        let Item::Test {
            name, body, span, ..
        } = item
        else {
            unreachable!("lower_test on non-test item");
        };
        let id = FnId(self.funcs.len() as u32);
        let display = match name {
            Some(n) => format!("test {n}"),
            None => "test".to_string(),
        };
        let ret = self.typed.arena.t_void();
        // Reserve a placeholder slot so `FnBuilder::new` can read name/inst/def.
        let mut placeholder = placeholder_fn(id, ret);
        placeholder.name = display;
        placeholder.inst = InstId {
            fn_def: DefId(u32::MAX),
            args: vec![InstArgKey::Int(id.0 as i128)],
        };
        placeholder.span = *span;
        self.funcs.push(placeholder);

        let mut fb = FnBuilder::new(self, id, ret, *span);
        fb.push_scope(None);
        fb.lower_block_stmts(body);
        if !fb.terminated() {
            fb.run_scope_exit_all(false);
            let v = Operand::Const(Const::Void);
            fb.set_term(Terminator::Return {
                value: v,
                err_trace: None,
            });
        }
        fb.pop_scope_no_defers();
        let alloc_info = fb.alloc_info.clone();
        let func = fb.finish();
        self.funcs[id.index()] = func;
        self.fn_alloc_info.insert(id, alloc_info);
        id
    }

    /// Builds a params-only stub for a v0.19 `extern` C function declaration: the
    /// parameter locals carry their declared types (so the codegen knows each
    /// arg's ABI class — e.g. a `[*:0]const u8` pointer vs a `[]const u8` slice),
    /// but the body is a trivial return (the symbol is undefined; the call site
    /// references it via a relocation, the body is never emitted). The
    /// extern/export/varargs linkage flags set by `enqueue` are preserved.
    fn extern_stub_function(
        &mut self,
        fid: FnId,
        inst: &InstId,
        params: &[k2_syntax::Param],
        span: Span,
    ) {
        let ret = self.funcs[fid.index()].ret;
        // Build the param locals (slot 0 is the return slot, like a real fn).
        let mut locals: Vec<Local> = Vec::with_capacity(params.len() + 1);
        locals.push(Local {
            id: LocalId(0),
            ty: ret,
            origin: LocalOrigin::Ret,
            address_taken: false,
            span,
        });
        let mut param_ids: Vec<LocalId> = Vec::with_capacity(params.len());
        for p in params {
            let pty = self.param_type(inst, p);
            let id = LocalId(locals.len() as u32);
            let origin = match self.def_of(p.span) {
                Some(d) => LocalOrigin::Param(d),
                None => LocalOrigin::Temp,
            };
            locals.push(Local {
                id,
                ty: pty,
                origin,
                address_taken: false,
                span: p.span,
            });
            param_ids.push(id);
        }
        let f = &mut self.funcs[fid.index()];
        f.locals = locals;
        f.params = param_ids;
        f.blocks.clear();
        let entry = f.new_block();
        f.entry = entry;
        let v = if matches!(self.typed.arena.get(ret), Type::Void) {
            Operand::Const(Const::Void)
        } else {
            Operand::Const(Const::Undef { ty: ret })
        };
        f.blocks[entry.index()].term = Terminator::Return {
            value: v,
            err_trace: None,
        };
    }

    /// Writes a trivial `return void` stub into `fid` (for a body-less fn).
    fn stub_function(&mut self, fid: FnId) {
        let ret = self.funcs[fid.index()].ret;
        let f = &mut self.funcs[fid.index()];
        f.locals.clear();
        f.blocks.clear();
        f.params.clear();
        let entry = f.new_block();
        f.entry = entry;
        let v = if matches!(self.typed.arena.get(ret), Type::Void) {
            Operand::Const(Const::Void)
        } else {
            Operand::Const(Const::Undef { ty: ret })
        };
        f.blocks[entry.index()].term = Terminator::Return {
            value: v,
            err_trace: None,
        };
    }

    // -------------------------------------------------------------------
    //  Type / name helpers for instantiation
    // -------------------------------------------------------------------

    /// Resolves a function definition's C linkage (v0.19) from the typed
    /// extern/export table: `(abi, linkage, is_extern_decl, varargs)`. A plain
    /// (non-FFI) function is `(K2, Internal, false, false)`.
    fn fn_linkage_of(&self, fn_def: DefId) -> (FnAbi, Linkage, bool, bool) {
        match self.typed.extern_fns.get(&fn_def) {
            Some(info) => match info.kind {
                k2_types::ExternKind::Extern => (
                    FnAbi::C,
                    Linkage::ExternC(info.abi_name.clone()),
                    true,
                    info.varargs,
                ),
                k2_types::ExternKind::Export => (
                    FnAbi::C,
                    Linkage::ExportC(info.abi_name.clone()),
                    false,
                    info.varargs,
                ),
            },
            None => (FnAbi::K2, Linkage::Internal, false, false),
        }
    }

    /// The result (success) type of an instantiated function.
    fn fn_ret_type(&self, inst: &InstId) -> TypeId {
        // For a method on an instantiated struct type, the receiver TypeId is the
        // first arg; the declared return type already references the concrete
        // Self/field types in the binding_types entry. Use the fn's binding type.
        if let Some(&fnty) = self.typed.binding_types.get(&inst.fn_def) {
            if let Type::Fn(sigid) = self.typed.arena.get(fnty) {
                return self.typed.arena.fnsigs[sigid.0 as usize].ret;
            }
        }
        self.typed.arena.t_void()
    }

    /// The type of parameter `p` in instantiation `inst`. For a non-generic fn we
    /// read the param's binding type; for a method on an instantiated type the
    /// checker already bound the concrete param types.
    fn param_type(&self, inst: &InstId, p: &k2_syntax::Param) -> TypeId {
        if let Some(def) = self.def_of(p.span) {
            if let Some(&t) = self.typed.binding_types.get(&def) {
                if !self.typed.arena.is_bottom(t) {
                    return t;
                }
            }
        }
        // Fall back to the fn signature's param type.
        if let Some(&fnty) = self.typed.binding_types.get(&inst.fn_def) {
            if let Type::Fn(sigid) = self.typed.arena.get(fnty) {
                let sig = &self.typed.arena.fnsigs[sigid.0 as usize];
                if let Some(info) = sig.params.iter().find(|pi| pi.span == p.span) {
                    return info.ty;
                }
            }
        }
        self.typed.arena.t_deferred()
    }

    /// A display name for an instantiation, e.g. `main`, `List(u32).push`.
    fn inst_display_name(&self, inst: &InstId) -> String {
        let base = self
            .resolved
            .defs
            .get(inst.fn_def.index())
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "fn".to_string());
        if inst.args.is_empty() {
            return base;
        }
        // Render the type args for a monomorphized name.
        let args: Vec<String> = inst
            .args
            .iter()
            .map(|a| match a {
                InstArgKey::Type(t) => self.typed.arena.fmt(*t),
                InstArgKey::Int(n) => n.to_string(),
                InstArgKey::Bool(b) => b.to_string(),
                InstArgKey::Str(s) => format!("{s:?}"),
            })
            .collect();
        format!("{base}[{}]", args.join(","))
    }

    /// `true` if `fn_def` is generic (has a `comptime`/`anytype` parameter), so
    /// it cannot be a standalone entry and must be monomorphized at call sites.
    fn fn_is_generic(&self, fn_def: DefId) -> bool {
        if let Some(&fnty) = self.typed.binding_types.get(&fn_def) {
            if let Type::Fn(sigid) = self.typed.arena.get(fnty) {
                let sig = &self.typed.arena.fnsigs[sigid.0 as usize];
                return sig.has_comptime_param || sig.has_anytype_param;
            }
        }
        false
    }

    /// Resolves a span to its DefId, if it names a binding site.
    fn def_of(&self, span: Span) -> Option<DefId> {
        // A definition's span equals its declaration site; the resolver records
        // it. We index the def table by span here.
        self.resolved
            .defs
            .iter()
            .find(|d| d.span == span)
            .map(|d| d.id)
    }

    /// Interns string-literal bytes and returns the ConstId.
    fn intern_str(&mut self, bytes: Vec<u8>) -> ConstId {
        if let Some(&id) = self.str_intern.get(&bytes) {
            return id;
        }
        let id = ConstId(self.consts.len() as u32);
        self.consts.push(ConstData::Bytes(bytes.clone()));
        self.str_intern.insert(bytes, id);
        id
    }

    /// Interns a stable, nonzero error tag for `name`.
    fn err_tag(&mut self, name: &str) -> ErrTag {
        if let Some(&t) = self.err_tags.get(name) {
            return t;
        }
        let tag = ErrTag((self.err_tags.len() as u16) + 1);
        self.err_tags.insert(name.to_string(), tag);
        tag
    }
}

/// A placeholder fn used transiently while moving a fn out of the program vec.
fn placeholder_fn(id: FnId, ret: TypeId) -> MirFunction {
    MirFunction {
        id,
        name: String::new(),
        def: None,
        abi: FnAbi::K2,
        linkage: Linkage::Internal,
        is_extern_decl: false,
        varargs: false,
        inst: InstId::plain(DefId(u32::MAX)),
        params: Vec::new(),
        ret,
        locals: Vec::new(),
        blocks: Vec::new(),
        entry: BlockId(0),
        panic_block: None,
        bool_ty: None,
        span: Span::default(),
    }
}

// =========================================================================
//  Indexing fn items
// =========================================================================

/// Recursively indexes `fn` items by their DefId so the worklist can fetch a
/// body. Generic functions whose body is a container live at the top level.
fn index_fn_items(resolved: &Resolved, item: &Item, out: &mut HashMap<DefId, Item>) {
    match item {
        Item::Fn { span, body, .. } => {
            if let Some(d) = resolved.defs.iter().find(|d| d.span == *span) {
                out.insert(d.id, item.clone());
            }
            // Index methods nested inside a generic's returned container.
            if let Some(stmts) = body {
                for s in stmts {
                    index_stmt_fns(resolved, s, out);
                }
            }
        }
        Item::Const { value, .. } => index_expr_fns(resolved, value, out),
        Item::Comptime { body, .. } => {
            for s in body {
                index_stmt_fns(resolved, s, out);
            }
        }
        _ => {}
    }
}

/// Recursively indexes *value-producing* `const` decls by DefId, descending into
/// container-type const values (`pub const testing = struct { ... }`) so a member
/// like `std.testing.allocator` can be inlined.
///
/// A const whose value is a [`Container`](Expr::Container), an
/// [`ErrorSet`](Expr::ErrorSet), or a type-expression is NOT a runtime value
/// (it is a namespace/type/error-set handled by the type system), so it is not
/// indexed — only consts that yield a real value (e.g. an `@allocHandle(...)`
/// capability) are.
fn index_value_consts(resolved: &Resolved, item: &Item, out: &mut HashMap<DefId, Expr>) {
    match item {
        Item::Const { value, span, .. } => {
            // Recurse into a container value to reach its nested member consts.
            if let Expr::Container(c) = value {
                for m in &c.members {
                    if let Member::Decl(inner) = m {
                        index_value_consts(resolved, inner, out);
                    }
                }
                return;
            }
            // A runtime value const: index it (skip namespaces/error-sets/types).
            if is_value_const_init(value) {
                if let Some(d) = resolved.defs.iter().find(|d| d.span == *span) {
                    out.insert(d.id, value.clone());
                }
            }
        }
        Item::Comptime { body, .. } => {
            for s in body {
                if let Stmt::Const {
                    value: Expr::Container(c),
                    ..
                } = s
                {
                    for m in &c.members {
                        if let Member::Decl(inner) = m {
                            index_value_consts(resolved, inner, out);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Indexes the DEFAULT initializers of every `struct` declared anywhere in `item`
/// (top-level, or nested as a member of another container), keyed by the struct's
/// defining span. A `const Sp = struct { x: i64 = 100, y: i64 };` records
/// `(Sp.span) -> [("x", 100)]`. Mirrors [`index_value_consts`]'s container walk.
fn index_struct_field_defaults(item: &Item, out: &mut HashMap<(u32, u32), Vec<(String, Expr)>>) {
    if let Item::Const { value, .. } = item {
        index_container_defaults(value, out);
    }
}

/// Walks a container-valued expression: if it is a `struct`, record its fields'
/// defaults; recurse into nested container-typed member decls (a struct declared
/// inside another type's body).
fn index_container_defaults(value: &Expr, out: &mut HashMap<(u32, u32), Vec<(String, Expr)>>) {
    let Expr::Container(c) = value else { return };
    let is_struct = matches!(c.kind, k2_syntax::ContainerKind::Struct { .. });
    if is_struct {
        let mut defaults = Vec::new();
        for m in &c.members {
            if let Member::Field(f) = m {
                if let Some(def) = &f.default {
                    defaults.push((f.name.clone(), def.clone()));
                }
            }
        }
        if !defaults.is_empty() {
            out.insert((c.span.start, c.span.end), defaults);
        }
    }
    // Nested container declarations (e.g. a struct defined inside another type).
    for m in &c.members {
        if let Member::Decl(Item::Const { value: inner, .. }) = m {
            index_container_defaults(inner, out);
        }
    }
}

/// `true` if a const's initializer expression yields a *runtime value* (rather
/// than a type / namespace / error-set). Conservative: only the expression
/// shapes that clearly denote a value are accepted, so a stray type-expression is
/// never mistaken for an inlinable value const.
fn is_value_const_init(value: &Expr) -> bool {
    !matches!(
        value,
        Expr::Container(_)
            | Expr::ErrorSet { .. }
            | Expr::ArrayType { .. }
            | Expr::FnType { .. }
            | Expr::ErrorUnion { .. }
            | Expr::AnyType { .. }
            | Expr::Pointer { .. }
            | Expr::Slice { .. }
            | Expr::ManyPtr { .. }
    )
}

/// Indexes fns nested in a statement (e.g. inside a returned `struct {...}`).
fn index_stmt_fns(resolved: &Resolved, stmt: &Stmt, out: &mut HashMap<DefId, Item>) {
    match stmt {
        Stmt::Return { value: Some(e), .. } | Stmt::Expr { expr: e, .. } => {
            index_expr_fns(resolved, e, out)
        }
        Stmt::Const { value, .. } => index_expr_fns(resolved, value, out),
        _ => {}
    }
}

/// Indexes fns nested inside a container-type expression value.
fn index_expr_fns(resolved: &Resolved, e: &Expr, out: &mut HashMap<DefId, Item>) {
    if let Expr::Container(c) = e {
        for m in &c.members {
            if let Member::Decl(item) = m {
                index_fn_items(resolved, item, out);
            }
        }
    }
}

// =========================================================================
//  The per-function builder
// =========================================================================

/// Per-function lowering state.
struct FnBuilder<'a, 'b> {
    /// The program-level lowerer.
    lo: &'a mut Lowerer<'b>,
    /// The function under construction.
    func: MirFunction,
    /// The block currently being appended to.
    cur: BlockId,
    /// Source binding DefId -> its local slot.
    locals_by_def: HashMap<DefId, LocalId>,
    /// The scope stack (defer/errdefer + loop/labeled-block targets).
    scopes: Vec<ScopeFrame>,
    /// Allocation/free facts for the leak pass.
    alloc_info: FnAllocInfo,
}

/// One lexical scope frame: its registered cleanup actions plus any loop/labeled-
/// block control targets it provides.
struct ScopeFrame {
    /// `defer`/`errdefer` actions in registration order; run LIFO on exit.
    defers: Vec<DeferKind>,
    /// Loop control targets, if this frame is a loop.
    loop_ctx: Option<LoopCtx>,
    /// A block/loop label, if any.
    label: Option<String>,
    /// The break-value slot + join block for a labeled block-as-expression.
    block_break: Option<(Option<LocalId>, BlockId)>,
}

/// A registered cleanup action.
enum DeferKind {
    /// `defer body` — runs on every scope exit.
    Defer(Stmt),
    /// `errdefer [|name|] body` — runs only on the error-return path.
    Errdefer {
        /// The optional `|err|` capture binding the in-flight error.
        capture: Option<DefId>,
        /// The cleanup body.
        body: Stmt,
    },
}

/// Loop control targets.
struct LoopCtx {
    /// Where `continue` jumps (the continue-expr block).
    continue_bb: BlockId,
    /// Where `break` jumps.
    break_bb: BlockId,
    /// The break-value slot, if the loop is used as an expression.
    break_val: Option<LocalId>,
}

impl<'a, 'b> FnBuilder<'a, 'b> {
    /// Builds a fresh function builder. The reserved fn (with its name/ret) is
    /// moved out of the program so we can fill it.
    fn new(lo: &'a mut Lowerer<'b>, fid: FnId, ret: TypeId, span: Span) -> Self {
        let name = lo.funcs[fid.index()].name.clone();
        let inst = lo.funcs[fid.index()].inst.clone();
        let def = lo.funcs[fid.index()].def;
        // Carry the C linkage/abi set by `enqueue` (an `export fn`'s body is
        // lowered here, so it keeps its `ExportC` linkage + `C` abi).
        let abi = lo.funcs[fid.index()].abi;
        let linkage = lo.funcs[fid.index()].linkage.clone();
        let is_extern_decl = lo.funcs[fid.index()].is_extern_decl;
        let varargs = lo.funcs[fid.index()].varargs;
        let bool_ty = lo.typed.arena.t_bool();
        let mut func = MirFunction {
            id: fid,
            name,
            def,
            abi,
            linkage,
            is_extern_decl,
            varargs,
            inst,
            params: Vec::new(),
            ret,
            locals: Vec::new(),
            blocks: Vec::new(),
            entry: BlockId(0),
            panic_block: None,
            bool_ty: Some(bool_ty),
            span,
        };
        // Slot 0 is always the return-value slot.
        func.locals.push(Local {
            id: LocalId(0),
            ty: ret,
            origin: LocalOrigin::Ret,
            address_taken: false,
            span,
        });
        let entry = func.new_block();
        func.entry = entry;
        FnBuilder {
            lo,
            func,
            cur: entry,
            locals_by_def: HashMap::new(),
            scopes: Vec::new(),
            alloc_info: FnAllocInfo::default(),
        }
    }

    /// Finalizes the function.
    fn finish(self) -> MirFunction {
        self.func
    }

    // ---- low-level building --------------------------------------------

    /// Appends a statement to the current block.
    fn emit(&mut self, s: Statement) {
        self.func.blocks[self.cur.index()].stmts.push(s);
    }

    /// Sets the terminator of the current block (only if not already set away
    /// from the default `Unreachable`).
    fn set_term(&mut self, t: Terminator) {
        self.func.blocks[self.cur.index()].term = t;
    }

    /// `true` if the current block already has a real (non-default) terminator.
    fn terminated(&self) -> bool {
        !matches!(
            self.func.blocks[self.cur.index()].term,
            Terminator::Unreachable
        )
    }

    /// Allocates a fresh block.
    fn new_block(&mut self) -> BlockId {
        self.func.new_block()
    }

    /// Allocates a fresh temporary local.
    fn new_temp(&mut self, ty: TypeId, span: Span) -> LocalId {
        self.func.new_temp(ty, span)
    }

    /// Adds a parameter local (slot order = declaration order, after the ret
    /// slot at index 0).
    fn add_param(&mut self, def: Option<DefId>, ty: TypeId, span: Span, name: &str) {
        let id = LocalId(self.func.locals.len() as u32);
        let origin = match def {
            Some(d) => LocalOrigin::Param(d),
            None => LocalOrigin::Temp,
        };
        self.func.locals.push(Local {
            id,
            ty,
            origin,
            address_taken: false,
            span,
        });
        self.func.params.push(id);
        if let Some(d) = def {
            self.locals_by_def.insert(d, id);
        }
        let _ = name;
    }

    /// Declares a local for a source binding (const/var/capture).
    fn declare_local(&mut self, def: Option<DefId>, ty: TypeId, span: Span) -> LocalId {
        let id = LocalId(self.func.locals.len() as u32);
        let origin = match def {
            Some(d) => LocalOrigin::Binding(d),
            None => LocalOrigin::Temp,
        };
        self.func.locals.push(Local {
            id,
            ty,
            origin,
            address_taken: false,
            span,
        });
        if let Some(d) = def {
            self.locals_by_def.insert(d, id);
        }
        self.emit(Statement::StorageLive(id));
        id
    }

    /// Marks a local's address as taken (drives the escape check).
    fn mark_address_taken(&mut self, base: LocalId) {
        self.func.locals[base.index()].address_taken = true;
    }

    /// The arena type recorded at `span`, or `Deferred` if none.
    fn type_at(&self, span: Span) -> TypeId {
        self.lo
            .typed
            .types
            .get(&(span.start, span.end))
            .copied()
            .unwrap_or_else(|| self.lo.typed.arena.t_deferred())
    }

    /// The `undef` carrier type for a value-position expression at `span`: the
    /// DENOTED type when `span` is a type-valued expression (so a type-denoting
    /// argument like `bool` / `[]const u8` passed to a `comptime T: type`
    /// intrinsic carries the concrete type, letting the VM honor a build option's
    /// declared kind), else the expression's own (meta) type.
    fn type_carrier_at(&self, span: Span) -> TypeId {
        self.lo
            .typed
            .type_valued_spans
            .get(&(span.start, span.end))
            .copied()
            .unwrap_or_else(|| self.type_at(span))
    }

    /// The enclosing instantiated struct type of the function currently being
    /// lowered, if it is a generic method (its [`InstId`] is keyed by a single
    /// `Type(struct_ty)` argument). Used to recover per-instantiation member
    /// resolutions so a comptime-type-param member dispatch resolves to the right
    /// target for THIS instantiation, not whichever one ran last in the checker.
    fn current_inst_struct_ty(&self) -> Option<TypeId> {
        match self.func.inst.args.as_slice() {
            [InstArgKey::Type(t)] => Some(*t),
            _ => None,
        }
    }

    /// The member resolution recorded at `span`, if any. When the current function
    /// is a generic-method instantiation, an instantiation-specific resolution
    /// (keyed by `(struct_ty, span)`) takes precedence over the span-only table —
    /// the latter is shared across instantiations and holds only the last writer.
    fn member_at(&self, span: Span) -> Option<MemberRes> {
        if let Some(struct_ty) = self.current_inst_struct_ty() {
            if let Some(res) = self
                .lo
                .typed
                .inst_members
                .get(&(struct_ty, (span.start, span.end)))
            {
                return Some(*res);
            }
        }
        self.lo.typed.members.get(&(span.start, span.end)).copied()
    }

    /// If `base.field` is a member access on a named error-set type
    /// (`ParseError.NotANumber`), returns the member name. The checker leaves
    /// such an access `Deferred` (the base is a `type` value), so the lowerer
    /// recognizes it here to produce a proper error *value* rather than an
    /// opaque intrinsic.
    pub(super) fn error_set_member(&self, base: &Expr, field: &str) -> Option<String> {
        if let Expr::Ident { span, .. } = base {
            let def = self.resolved_def(*span)?;
            if let Some(members) = self.lo.err_set_consts.get(&def) {
                if members.iter().any(|m| m == field) {
                    return Some(field.to_string());
                }
            }
        }
        None
    }

    // ---- scopes / defers -----------------------------------------------

    /// Pushes a fresh scope frame with an optional label.
    fn push_scope(&mut self, label: Option<String>) {
        self.scopes.push(ScopeFrame {
            defers: Vec::new(),
            loop_ctx: None,
            label,
            block_break: None,
        });
    }

    /// Pops the innermost scope frame WITHOUT running its defers (used after the
    /// caller already emitted the exit, or for the fn root).
    fn pop_scope_no_defers(&mut self) {
        self.scopes.pop();
    }

    /// Runs the defers of the innermost scope (fall-through exit) and pops it.
    fn pop_scope_run_defers(&mut self) {
        let frame_idx = self.scopes.len() - 1;
        self.run_defers_in_frame(frame_idx, false);
        self.scopes.pop();
    }

    /// Runs all defer/errdefer actions of frame `idx` in LIFO order.
    /// `is_error` selects whether `errdefer` actions fire.
    fn run_defers_in_frame(&mut self, idx: usize, is_error: bool) {
        // Clone the action list so we can mutate the builder while lowering.
        let actions: Vec<usize> = (0..self.scopes[idx].defers.len()).rev().collect();
        for k in actions {
            // Re-borrow each step (the vec is stable during this loop).
            let run = match &self.scopes[idx].defers[k] {
                DeferKind::Defer(_) => true,
                DeferKind::Errdefer { .. } => is_error,
            };
            if !run {
                continue;
            }
            self.emit(Statement::Note(format!(
                "defer #{} ({})",
                k,
                if is_error { "error path" } else { "exit" }
            )));
            // Extract the body (clone the Stmt) to lower it inline. An errdefer
            // with an `|err|` capture binds the in-flight error value; the VM
            // substitutes the actual error on the error-return path.
            let (body, cap) = match &self.scopes[idx].defers[k] {
                DeferKind::Defer(b) => (b.clone(), None),
                DeferKind::Errdefer { body, capture } => (body.clone(), *capture),
            };
            if let Some(cap_def) = cap {
                let err_ty = self
                    .lo
                    .typed
                    .binding_types
                    .get(&cap_def)
                    .copied()
                    .unwrap_or_else(|| self.lo.typed.arena.t_anyerror());
                let local = self.declare_local(Some(cap_def), err_ty, Span::default());
                self.assign(
                    Place::local(local),
                    Rvalue::Use(Operand::Const(Const::Undef { ty: err_ty })),
                    Span::default(),
                );
            }
            self.lower_stmt(&body);
        }
    }

    /// Runs every enclosing scope's defers from the innermost outward (used by
    /// `return`). `is_error` selects the error path.
    fn run_scope_exit_all(&mut self, is_error: bool) {
        for idx in (0..self.scopes.len()).rev() {
            self.run_defers_in_frame(idx, is_error);
        }
    }

    /// Runs defers for the scopes from the innermost down to (and excluding)
    /// `target_idx` (used by `break`/`continue`).
    fn run_defers_until(&mut self, target_idx: usize) {
        let mut idx = self.scopes.len();
        while idx > target_idx {
            idx -= 1;
            self.run_defers_in_frame(idx, false);
        }
    }

    // ---- statement / block lowering ------------------------------------

    /// Lowers a sequence of statements that share one lexical scope, opening and
    /// closing a scope frame (so block-local defers fire on fall-through).
    fn lower_block_scoped(&mut self, stmts: &[Stmt]) {
        self.push_scope(None);
        self.lower_block_stmts(stmts);
        if !self.terminated() {
            self.pop_scope_run_defers();
        } else {
            self.pop_scope_no_defers();
        }
    }

    /// Lowers a sequence of statements into the current scope frame (no new
    /// frame); used for the fn body and defer-inlined bodies.
    fn lower_block_stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            if self.terminated() {
                break; // dead code after a terminator
            }
            self.lower_stmt(s);
        }
    }

    /// Lowers one statement.
    fn lower_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Const { value, span, .. }
            | Stmt::Var {
                value: Some(value),
                span,
                ..
            } => {
                let def = self.binding_def_at(*span);
                let ty = def
                    .and_then(|d| self.lo.typed.binding_types.get(&d).copied())
                    .unwrap_or_else(|| self.type_at(value.span()));
                // Comptime-folded const: inline a literal, emit no runtime code.
                if let Some(folded) = self.folded_const(def, value) {
                    let local = self.declare_local(def, ty, *span);
                    self.emit(Statement::Assign {
                        place: Place::local(local),
                        rvalue: Rvalue::Use(Operand::Const(folded)),
                        span: *span,
                    });
                    return;
                }
                let local = self.declare_local(def, ty, *span);
                self.lower_into(Place::local(local), value);
                self.record_alloc_binding(local, value);
            }
            Stmt::Var {
                value: None, span, ..
            } => {
                let def = self.binding_def_at(*span);
                let ty = def
                    .and_then(|d| self.lo.typed.binding_types.get(&d).copied())
                    .unwrap_or_else(|| self.lo.typed.arena.t_deferred());
                self.declare_local(def, ty, *span);
            }
            Stmt::Assign {
                target,
                op,
                value,
                span,
            } => self.lower_assign(target, *op, value, *span),
            Stmt::Return { value, span } => self.lower_return(value.as_ref(), *span),
            Stmt::Expr { expr, .. } => {
                // Lower for effect; discard the result.
                self.lower_for_effect(expr);
            }
            Stmt::Defer { body, .. } => {
                self.scopes
                    .last_mut()
                    .unwrap()
                    .defers
                    .push(DeferKind::Defer((**body).clone()));
                self.note_release_from_defer(body);
            }
            Stmt::Errdefer {
                capture,
                body,
                span,
            } => {
                let cap_def = capture.as_ref().and_then(|_| self.capture_def_at(*span));
                self.scopes
                    .last_mut()
                    .unwrap()
                    .defers
                    .push(DeferKind::Errdefer {
                        capture: cap_def,
                        body: (**body).clone(),
                    });
                self.note_release_from_defer(body);
            }
            Stmt::Block { body, .. } => self.lower_block_scoped(body),
            Stmt::Comptime { body, .. } => {
                // A comptime block is folded at type-check and emits no runtime code
                // — EXCEPT a `return`, which returns a comptime-known value from the
                // enclosing function (`fn f(comptime n) u64 { comptime { …; return r; } }`).
                // Without emitting it, the function falls through to `undef` (prints
                // `<int>`). Emit the folded return value.
                if let Some(Stmt::Return { value, span }) =
                    body.iter().find(|s| matches!(s, Stmt::Return { .. }))
                {
                    self.lower_comptime_return(value.as_ref(), *span);
                }
            }
            Stmt::If { expr, .. }
            | Stmt::While { expr, .. }
            | Stmt::For { expr, .. }
            | Stmt::Switch { expr, .. } => {
                self.lower_for_effect(expr);
            }
            Stmt::Break { label, value, .. } => self.lower_break(label.as_deref(), value.as_ref()),
            Stmt::Continue { label, .. } => self.lower_continue(label.as_deref()),
        }
    }

    /// Lowers a `return <value>` that sits inside a `comptime { … }` block: the
    /// value is comptime-known, so materialize its folded constant (the block's
    /// runtime statements were elided). Falls back to lowering the expression when
    /// no folded constant is recorded (e.g. a `return` of a comptime-known
    /// aggregate, which still lowers structurally).
    fn lower_comptime_return(&mut self, value: Option<&Expr>, span: Span) {
        let ret_ty = self.func.ret;
        let operand = match value {
            Some(e) => {
                if let Some(c) = self.comptime_span_const(e.span(), ret_ty) {
                    Operand::Const(c)
                } else {
                    let tmp = self.new_temp(ret_ty, span);
                    self.lower_return_value_into(Place::local(tmp), e, ret_ty);
                    Operand::local(tmp)
                }
            }
            None => Operand::Const(Const::Void),
        };
        self.run_scope_exit_all(false);
        self.emit(Statement::Assign {
            place: Place::local(LocalId(0)),
            rvalue: Rvalue::Use(operand.clone()),
            span,
        });
        self.set_term(Terminator::Return {
            value: operand,
            err_trace: None,
        });
    }

    /// Lowers a `return [value]`, running every enclosing scope's defers.
    fn lower_return(&mut self, value: Option<&Expr>, span: Span) {
        let ret_ty = self.func.ret;
        // Compute the returned value first (before defers, so defers see the
        // post-value state but the value is already materialized in a temp).
        let (operand, is_error, is_origin) = match value {
            Some(e) => {
                let vty = self.type_at(e.span());
                let is_err = self.is_error_value(e, vty);
                // This `return` is the ORIGIN of the error — the site where the
                // error value first appears (spec §6.9) — exactly when it lowers
                // to a `MakeErr`: an error-union return whose operand is a
                // statically-known `error.X` / `Set.X` tag. Pass-through returns
                // of an existing error union (`return someEU`) are not origins.
                let is_origin = self.is_error_origin_return(e, ret_ty, vty);
                // Materialize into a temp so defers can run after.
                let tmp = self.new_temp(ret_ty, span);
                self.lower_return_value_into(Place::local(tmp), e, ret_ty);
                (Operand::local(tmp), is_err, is_origin)
            }
            None => (Operand::Const(Const::Void), false, false),
        };
        self.run_scope_exit_all(is_error);
        // Move into the ret slot, then Return.
        self.emit(Statement::Assign {
            place: Place::local(LocalId(0)),
            rvalue: Rvalue::Use(operand.clone()),
            span,
        });
        // Seed the error-return trace's ORIGIN frame at the creation site so the
        // deepest printed frame is where the error came from (`return E.X`), not
        // the first `try` above it. The VM's `MakeErr` clears the per-fiber trace
        // buffer (a fresh error starts clean), then this `ReturnErr` pushes the
        // origin frame; later `try` sites append above it, newest-first.
        self.set_term(Terminator::Return {
            value: operand,
            err_trace: if is_origin { Some(span) } else { None },
        });
    }

    /// `true` when a `return e` is the error *origin* — i.e. it lowers to a
    /// `MakeErr` that mints a fresh error union from a statically-known
    /// `error.X` / `Set.X` tag. This mirrors the `MakeErr` arm of
    /// [`lower_return_value_into`]; a pass-through of an existing error union or a
    /// dynamic error-set value is not an origin (no fresh error is created here).
    fn is_error_origin_return(&mut self, e: &Expr, ret_ty: TypeId, vty: TypeId) -> bool {
        if !matches!(self.lo.typed.arena.get(ret_ty), Type::ErrorUnion { .. }) {
            return false;
        }
        self.is_error_value(e, vty) && self.error_operand_tag(e).is_some()
    }

    /// Lowers the value of a `return` into `dst`, wrapping into the error union /
    /// optional as the return type requires.
    fn lower_return_value_into(&mut self, dst: Place, e: &Expr, ret_ty: TypeId) {
        let ret = self.lo.typed.arena.get(ret_ty).clone();
        let vty = self.type_at(e.span());
        // error-union return with an error operand -> MakeErr; with an ok value
        // -> MakeOk (unless the value is already an error union being returned).
        if let Type::ErrorUnion { ok, .. } = ret {
            if self.is_error_value(e, vty) {
                if let Some(tag) = self.error_operand_tag(e) {
                    self.emit(Statement::Assign {
                        place: dst,
                        rvalue: Rvalue::MakeErr(tag, ret_ty),
                        span: e.span(),
                    });
                    return;
                }
            }
            // If the value itself is an error union (e.g. `return someEU`), pass
            // it through; else wrap the ok value.
            if matches!(self.lo.typed.arena.get(vty), Type::ErrorUnion { .. }) {
                self.lower_into(dst, e);
                return;
            }
            let okp = self.new_temp(ok, e.span());
            self.lower_into(Place::local(okp), e);
            self.emit(Statement::Assign {
                place: dst,
                rvalue: Rvalue::MakeOk(Operand::local(okp), ret_ty),
                span: e.span(),
            });
            return;
        }
        self.lower_into(dst, e);
    }

    /// `true` if `e` evaluates to an error value (an `error.X` literal, a
    /// `Set.Member` access on a named error set, or a value of error-set type)
    /// being returned on the error path.
    fn is_error_value(&self, e: &Expr, vty: TypeId) -> bool {
        if matches!(e, Expr::ErrorLiteral { .. }) {
            return true;
        }
        if let Expr::Field { base, field, .. } = e {
            if self.error_set_member(base, field).is_some() {
                return true;
            }
        }
        matches!(
            self.lo.typed.arena.get(vty),
            Type::ErrorSet(_) | Type::AnyError
        )
    }

    /// The error tag of an `error.X`/`Set.X` operand, if statically known.
    fn error_operand_tag(&mut self, e: &Expr) -> Option<ErrTag> {
        match e {
            Expr::ErrorLiteral { name, .. } => Some(self.lo.err_tag(name)),
            Expr::Field { base, field, .. } => {
                if self.error_set_member(base, field).is_some() {
                    Some(self.lo.err_tag(field))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Lowers `target op= value;`.
    fn lower_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, span: Span) {
        let place = self.lower_place(target);
        if op == AssignOp::Eq {
            self.lower_into(place, value);
            return;
        }
        // Expand `a op= b` -> `a = a op b`, routing through the checked path.
        let bin = compound_binop(op);
        let ty = self.type_at(target.span());
        let lhs = Operand::Copy(place.clone());
        let rhs_ty = self.type_at(value.span());
        let rhs = self.lower_operand(value, rhs_ty);
        let rv = self.checked_binary(bin, lhs, rhs, ty, span);
        self.emit(Statement::Assign {
            place,
            rvalue: rv,
            span,
        });
    }

    // ---- expression -> place / operand / into --------------------------

    /// Lowers an expression purely for its side effects (discarding its value).
    fn lower_for_effect(&mut self, e: &Expr) {
        match e {
            Expr::If { .. } => self.lower_if(e, None),
            Expr::While { .. } => self.lower_while(e, None),
            Expr::For { .. } => self.lower_for(e, None),
            Expr::Switch { .. } => self.lower_switch(e, None),
            Expr::Block { label, body, .. } => {
                self.lower_block_expr(label.as_deref(), body, None);
            }
            Expr::Unary {
                op: AstUnOp::Try,
                operand,
                span,
            } => {
                // `try e;` for effect: still must propagate the error.
                let _ = self.lower_try(operand, *span);
            }
            _ => {
                // Lower into a throwaway temp (captures call/intrinsic effects).
                let ty = self.type_at(e.span());
                let tmp = self.new_temp(ty, e.span());
                self.lower_into(Place::local(tmp), e);
            }
        }
    }

    /// Lowers an expression as an [`Operand`] of (expected) type `ty`.
    fn lower_operand(&mut self, e: &Expr, ty: TypeId) -> Operand {
        // Fast paths for pure constants / place reads.
        match e {
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Char { .. }
            | Expr::Bool { .. }
            | Expr::Null { .. }
            | Expr::Undefined { .. }
            | Expr::ErrorLiteral { .. }
            | Expr::EnumLiteral { .. } => {
                if let Some(c) = self.const_of(e, ty) {
                    return Operand::Const(c);
                }
            }
            Expr::Ident { .. } | Expr::Field { .. } | Expr::Index { .. } | Expr::Deref { .. } => {
                if let Some(p) = self.try_lower_place(e) {
                    return Operand::Copy(p);
                }
                // A top-level value const has no local slot: inline its
                // initializer into a temp rather than falling through to `undef`.
                if let Some(init) = self.top_level_const_init(e) {
                    let t = if self.lo.typed.arena.is_bottom(ty) {
                        self.type_at(e.span())
                    } else {
                        ty
                    };
                    let tmp = self.new_temp(t, e.span());
                    self.lower_into(Place::local(tmp), &init);
                    return Operand::local(tmp);
                }
            }
            _ => {}
        }
        // General path: lower into a temp.
        let t = if self.lo.typed.arena.is_bottom(ty) {
            self.type_at(e.span())
        } else {
            ty
        };
        let tmp = self.new_temp(t, e.span());
        self.lower_into(Place::local(tmp), e);
        Operand::local(tmp)
    }

    /// Lowers an expression as a [`Place`] (an lvalue). Panics-free: a
    /// non-lvalue expression is materialized into a temp first.
    fn lower_place(&mut self, e: &Expr) -> Place {
        if let Some(p) = self.try_lower_place(e) {
            return p;
        }
        let ty = self.type_at(e.span());
        let tmp = self.new_temp(ty, e.span());
        self.lower_into(Place::local(tmp), e);
        Place::local(tmp)
    }

    /// Tries to lower `e` as a place; returns `None` if `e` is not an lvalue.
    fn try_lower_place(&mut self, e: &Expr) -> Option<Place> {
        match e {
            Expr::Ident { span, .. } => {
                let def = self.resolved_def(*span)?;
                let local = self.locals_by_def.get(&def).copied()?;
                Some(Place::local(local))
            }
            Expr::Field { base, field, span } => {
                let member = self.member_at(*span);
                match member {
                    Some(MemberRes::Field(idx)) => {
                        let base_place = self.lower_place_autoderef(base);
                        let fty = self.type_at(*span);
                        Some(base_place.project(Proj::Field {
                            index: idx,
                            ty: fty,
                            packed: None,
                        }))
                    }
                    Some(MemberRes::PackedField(idx, pf)) => {
                        // A `packed struct` field: carry the bit descriptor so the
                        // VM/native do a shift+mask at the correct offset/width.
                        let base_place = self.lower_place_autoderef(base);
                        let fty = self.type_at(*span);
                        Some(base_place.project(Proj::Field {
                            index: idx,
                            ty: fty,
                            packed: Some(pf),
                        }))
                    }
                    Some(MemberRes::BuiltinField) => {
                        // `.len` / `.ptr` of a slice.
                        let base_place = self.lower_place_autoderef(base);
                        let fty = self.type_at(*span);
                        let which = if field == "ptr" {
                            SliceMeta::Ptr
                        } else {
                            SliceMeta::Len
                        };
                        Some(base_place.project(Proj::SliceMeta { which, ty: fty }))
                    }
                    _ => None,
                }
            }
            Expr::Index { base, index, span } => {
                let base_place = self.lower_place_autoderef(base);
                // The element type must come from the BASE collection, not from
                // the index expression's recorded type. Bidirectional checking
                // can stamp a *coerced* type at the index span (e.g. the operand
                // of `@as(u32, s[0])` records u32), and the native backend would
                // then size the element memory load by that wrong width — reading
                // 4 bytes for a `[]const u8` element and yielding garbage. Derive
                // it from the slice/array/vector element; fall back to the span
                // type only if the base is not a recognizable collection.
                let span_ty = self.type_at(*span);
                let base_ty = self.type_at(base.span());
                let elem_ty = {
                    let arena = &self.lo.typed.arena;
                    let peeled = match arena.get(base_ty) {
                        Type::Pointer { pointee, .. } => *pointee,
                        _ => base_ty,
                    };
                    match arena.get(peeled) {
                        Type::Slice { elem, .. }
                        | Type::Array { elem, .. }
                        | Type::Vector { elem, .. } => *elem,
                        _ => span_ty,
                    }
                };
                let idx_ty = self.lo.typed.arena.t_usize();
                let idx = self.lower_operand(index, idx_ty);
                self.emit_bounds_check(&base_place, base, &idx, *span);
                Some(base_place.project(Proj::Index {
                    index: idx,
                    ty: elem_ty,
                }))
            }
            Expr::Deref { base, .. } => {
                let bp = self.lower_place(base);
                Some(bp.project(Proj::Deref))
            }
            _ => None,
        }
    }

    /// Lowers a base place, auto-dereferencing one pointer layer if the base is a
    /// `*T` (mirroring the checker's `synth_field` auto-deref).
    fn lower_place_autoderef(&mut self, base: &Expr) -> Place {
        let bp = self.lower_place(base);
        let bty = self.type_at(base.span());
        if matches!(self.lo.typed.arena.get(bty), Type::Pointer { .. }) {
            bp.project(Proj::Deref)
        } else {
            bp
        }
    }

    /// Resolves an identifier span to its DefId via the uses table.
    fn resolved_def(&self, span: Span) -> Option<DefId> {
        match self.lo.resolved.uses.at(span).map(|u| u.res) {
            Some(Resolution::Def(d)) | Some(Resolution::Predeclared(d)) => Some(d),
            _ => None,
        }
    }

    /// The DefId of the binding declared at `span` (a const/var/local site).
    fn binding_def_at(&self, span: Span) -> Option<DefId> {
        self.lo.def_of(span)
    }

    /// The DefId of an errdefer/catch capture declared near `span`.
    fn capture_def_at(&self, span: Span) -> Option<DefId> {
        // A capture's def is recorded by the resolver at the capture name span;
        // for errdefer the capture lives within the statement span. We look for a
        // Capture def whose span is inside the statement span.
        self.lo
            .resolved
            .defs
            .iter()
            .find(|d| {
                matches!(d.kind, k2_resolve::DefKind::Capture)
                    && d.span.start >= span.start
                    && d.span.end <= span.end
            })
            .map(|d| d.id)
    }
}

/// Maps a compound assignment operator to its binary operator.
fn compound_binop(op: AssignOp) -> BinOp {
    match op {
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
        AssignOp::Eq => BinOp::Add, // unreachable; Eq handled separately
    }
}

// Note: the bulk of expression lowering (`lower_into`), control flow, calls,
// intrinsics, constants, and safety-check helpers live in `lower_expr.rs`.
mod lower_expr;
mod lower_flow;
