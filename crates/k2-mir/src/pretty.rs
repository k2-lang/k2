//! The MIR pretty-printer: a stable, greppable, one-statement-per-line dump.
//!
//! [`dump_mir`] renders a whole [`MirProgram`] as text — each function's locals,
//! basic blocks, statements, inserted safety checks, intrinsic calls, and defer
//! expansions. The format is the readable artifact the `k2c mir` subcommand
//! prints and the acceptance tests grep: a Debug-mode dump shows `*_check` lines
//! and the diff against a `ReleaseFast` dump is exactly the safety scaffolding.

use std::fmt::Write;

use crate::ir::*;
use k2_types::TypeArena;

/// Renders a whole program as a readable MIR dump.
pub fn dump_mir(prog: &MirProgram) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# MIR dump — {} fn(s), {} block(s), {} check(s), mode {:?}",
        prog.funcs.len(),
        prog.block_count(),
        prog.check_count(),
        prog.mode
    );
    for func in &prog.funcs {
        dump_fn(&mut out, prog, func);
        let _ = writeln!(out);
    }
    out
}

/// Renders one function.
fn dump_fn(out: &mut String, prog: &MirProgram, func: &MirFunction) {
    let arena = &prog.arena;
    // Signature line.
    let params: Vec<String> = func
        .params
        .iter()
        .map(|p| {
            format!(
                "{}: {}",
                local_name(func, *p),
                arena.fmt(func.locals[p.index()].ty)
            )
        })
        .collect();
    let _ = writeln!(
        out,
        "fn {}({}) -> {} {{",
        func.name,
        params.join(", "),
        arena.fmt(func.ret)
    );
    // Local table.
    for l in &func.locals {
        let kind = match l.origin {
            LocalOrigin::Ret => "ret",
            LocalOrigin::Param(_) => "param",
            LocalOrigin::Binding(_) => "let",
            LocalOrigin::Temp => "tmp",
        };
        let addr = if l.address_taken { " &" } else { "" };
        let _ = writeln!(
            out,
            "  let {} : {}   // {kind}{addr}",
            local_id(l.id),
            arena.fmt(l.ty)
        );
    }
    // Blocks.
    for b in &func.blocks {
        let tag = if b.is_panic { "  (panic)" } else { "" };
        let _ = writeln!(out, "  bb{}:{tag}", b.id.0);
        for s in &b.stmts {
            dump_stmt(out, arena, s);
        }
        dump_term(out, &b.term);
    }
    let _ = writeln!(out, "}}");
}

/// Renders a statement.
fn dump_stmt(out: &mut String, arena: &TypeArena, s: &Statement) {
    match s {
        Statement::Assign { place, rvalue, .. } => {
            let _ = writeln!(
                out,
                "    {} = {}",
                fmt_place(place),
                fmt_rvalue(arena, rvalue)
            );
        }
        Statement::Eval { rvalue, .. } => {
            let _ = writeln!(out, "    eval {}", fmt_rvalue(arena, rvalue));
        }
        Statement::Check(c) => {
            let _ = writeln!(out, "    {}", fmt_check(arena, c));
        }
        Statement::StorageLive(l) => {
            let _ = writeln!(out, "    storage_live {}", local_id(*l));
        }
        Statement::StorageDead(l) => {
            let _ = writeln!(out, "    storage_dead {}", local_id(*l));
        }
        Statement::Note(n) => {
            let _ = writeln!(out, "    // {n}");
        }
    }
}

/// Renders a terminator.
fn dump_term(out: &mut String, t: &Terminator) {
    match t {
        Terminator::Goto(b) => {
            let _ = writeln!(out, "    goto -> bb{}", b.0);
        }
        Terminator::Branch {
            cond,
            then_bb,
            else_bb,
        } => {
            let _ = writeln!(
                out,
                "    branch {} -> bb{}, bb{}",
                fmt_operand(cond),
                then_bb.0,
                else_bb.0
            );
        }
        Terminator::Switch {
            scrutinee,
            targets,
            default,
        } => {
            let arms: Vec<String> = targets
                .iter()
                .map(|(v, b)| format!("{v} => bb{}", b.0))
                .collect();
            let _ = writeln!(
                out,
                "    switch {} [{}] default -> bb{}",
                fmt_operand(scrutinee),
                arms.join(", "),
                default.0
            );
        }
        Terminator::Return { value } => {
            let _ = writeln!(out, "    return {}", fmt_operand(value));
        }
        Terminator::Trap { reason } => {
            let _ = writeln!(out, "    trap {reason:?}");
        }
        Terminator::Unreachable => {
            let _ = writeln!(out, "    unreachable");
        }
    }
}

/// Renders a safety check (the `*_check` lines the acceptance gate greps).
fn fmt_check(arena: &TypeArena, c: &SafetyCheck) -> String {
    match &c.kind {
        CheckKind::Bounds { index, len } => {
            format!(
                "bounds_check {} < {} else -> panic",
                fmt_operand(index),
                fmt_operand(len)
            )
        }
        CheckKind::SliceRange { lo, hi, len } => {
            format!(
                "bounds_check {} <= {} <= {} else -> panic",
                fmt_operand(lo),
                fmt_operand(hi),
                fmt_operand(len)
            )
        }
        CheckKind::AddOverflow { op, a, b, ty } => {
            let name = match op {
                ArithOp::Add => "add",
                ArithOp::Sub => "sub",
                ArithOp::Mul => "mul",
            };
            format!(
                "overflow_check {name} {}, {} : {} else -> panic",
                fmt_operand(a),
                fmt_operand(b),
                arena.fmt(*ty)
            )
        }
        CheckKind::DivByZero { b, .. } => {
            format!("divzero_check {} != 0 else -> panic", fmt_operand(b))
        }
        CheckKind::DivOverflow { a, b, ty } => {
            format!(
                "divoverflow_check {} / {} : {} else -> panic",
                fmt_operand(a),
                fmt_operand(b),
                arena.fmt(*ty)
            )
        }
        CheckKind::NegOverflow { a, ty } => {
            format!(
                "overflow_check neg {} : {} else -> panic",
                fmt_operand(a),
                arena.fmt(*ty)
            )
        }
        CheckKind::NarrowFits { value, ty } => {
            format!(
                "narrow_check {} fits {} else -> panic",
                fmt_operand(value),
                arena.fmt(*ty)
            )
        }
        CheckKind::LenEq { a, b } => {
            format!(
                "len_eq_check {} == {} else -> panic",
                fmt_operand(a),
                fmt_operand(b)
            )
        }
        CheckKind::Unreachable => "unreachable_check else -> panic".to_string(),
    }
}

/// Renders an rvalue.
fn fmt_rvalue(arena: &TypeArena, rv: &Rvalue) -> String {
    match rv {
        Rvalue::Use(o) => fmt_operand(o),
        Rvalue::Binary { op, lhs, rhs, .. } => {
            format!(
                "{} {}, {}",
                binop_name(*op),
                fmt_operand(lhs),
                fmt_operand(rhs)
            )
        }
        Rvalue::Unary { op, operand, .. } => {
            format!("{} {}", unop_name(*op), fmt_operand(operand))
        }
        Rvalue::Ref {
            place, is_const, ..
        } => {
            format!(
                "&{}{}",
                if *is_const { "const " } else { "" },
                fmt_place(place)
            )
        }
        Rvalue::Cast { kind, operand, ty } => {
            format!(
                "cast.{kind:?} {} : {}",
                fmt_operand(operand),
                arena.fmt(*ty)
            )
        }
        Rvalue::MakeSlice {
            ptr, offset, len, ..
        } => {
            format!(
                "make_slice {{ ptr: {}, offset: {}, len: {} }}",
                fmt_operand(ptr),
                fmt_operand(offset),
                fmt_operand(len)
            )
        }
        Rvalue::MakeSome(o, _) => format!("some {}", fmt_operand(o)),
        Rvalue::MakeNull(_) => "null".to_string(),
        Rvalue::MakeOk(o, _) => format!("ok {}", fmt_operand(o)),
        Rvalue::MakeErr(tag, _) => format!("err #{}", tag.0),
        Rvalue::Discriminant { operand, kind } => {
            format!("discr.{kind:?} {}", fmt_operand(operand))
        }
        Rvalue::Aggregate { kind, fields, .. } => {
            let fs: Vec<String> = fields.iter().map(fmt_operand).collect();
            format!("{kind:?} {{ {} }}", fs.join(", "))
        }
        Rvalue::Call { func, args, .. } => {
            let a: Vec<String> = args.iter().map(fmt_operand).collect();
            format!("call fn{}({})", func.0, a.join(", "))
        }
        Rvalue::Intrinsic { path, args, .. } => {
            let a: Vec<String> = args.iter().map(fmt_operand).collect();
            format!("intrinsic {}({})", fmt_intrinsic_path(path), a.join(", "))
        }
    }
}

/// Renders an intrinsic path, including its root.
fn fmt_intrinsic_path(path: &IntrinsicPath) -> String {
    let root = match &path.root {
        IntrinsicRoot::Module(def) => format!("module#{}", def.0),
        IntrinsicRoot::Value(op) => format!("value({})", fmt_operand(op)),
        IntrinsicRoot::Builtin(name) => format!("@{name}"),
    };
    if path.members.is_empty() {
        root
    } else {
        format!("{root}.{}", path.dotted())
    }
}

/// Renders an operand.
fn fmt_operand(o: &Operand) -> String {
    match o {
        Operand::Copy(p) => fmt_place(p),
        Operand::Const(c) => fmt_const(c),
    }
}

/// Renders a place.
fn fmt_place(p: &Place) -> String {
    let mut s = local_id(p.base);
    for proj in &p.proj {
        match proj {
            Proj::Deref => s = format!("(*{s})"),
            Proj::Field { index, .. } => s = format!("{s}.f{index}"),
            Proj::Index { index, .. } => s = format!("{s}[{}]", fmt_operand(index)),
            Proj::SliceMeta { which, .. } => {
                let m = match which {
                    SliceMeta::Ptr => "ptr",
                    SliceMeta::Len => "len",
                };
                s = format!("{s}.{m}");
            }
            Proj::Payload { .. } => s = format!("{s}.payload"),
        }
    }
    s
}

/// Renders a constant.
fn fmt_const(c: &Const) -> String {
    match c {
        Const::Int { value, .. } => format!("{value}"),
        Const::Float { bits, .. } => format!("f64:{:#x}", bits),
        Const::Bool(b) => b.to_string(),
        Const::Void => "()".to_string(),
        Const::Str(id) => format!("str#{}", id.0),
        Const::EnumVal { variant, .. } => format!(".v{variant}"),
        Const::ErrVal { tag, .. } => format!("error#{}", tag.0),
        Const::EmptySlice { .. } => "&.{}".to_string(),
        Const::Undef { .. } => "undef".to_string(),
        Const::Aggregate { id, .. } => format!("agg#{}", id.0),
        Const::FnRef(f) => format!("fn#{}", f.0),
    }
}

/// The display id of a local (e.g. `_3`).
fn local_id(l: LocalId) -> String {
    format!("_{}", l.0)
}

/// A local's display id (used in the local table and signature).
fn local_name(func: &MirFunction, l: LocalId) -> String {
    let _ = func;
    local_id(l)
}

/// The mnemonic for a binary operator.
fn binop_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        BinOp::BitAnd => "and",
        BinOp::BitOr => "or",
        BinOp::BitXor => "xor",
        BinOp::Shl => "shl",
        BinOp::Shr => "shr",
        BinOp::Eq => "eq",
        BinOp::Ne => "ne",
        BinOp::Lt => "lt",
        BinOp::Le => "le",
        BinOp::Gt => "gt",
        BinOp::Ge => "ge",
    }
}

/// The mnemonic for a unary operator.
fn unop_name(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "neg",
        UnOp::BitNot => "bitnot",
        UnOp::Not => "not",
    }
}
