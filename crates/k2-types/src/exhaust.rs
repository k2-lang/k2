//! Switch typing and exhaustiveness: over a concrete `enum` or error set the
//! checker verifies that every variant/member is covered (or an `else` arm
//! exists), and flags duplicate arms and an unreachable `else`.
//!
//! Over an integer scrutinee an `else` arm is required (full integer
//! exhaustiveness is not computed). Over a `Deferred`/`anytype` scrutinee no
//! exhaustiveness diagnostic is produced — the arms are still typed and joined.

use std::collections::HashSet;

use k2_syntax::{Expr, Span, SwitchArm, SwitchItem, SwitchPattern};

use crate::ty::{Type, TypeId};
use crate::Diagnostic;

impl crate::check::Checker<'_> {
    /// Types a `switch` expression/statement: type the scrutinee, bind each arm's
    /// payload capture, join the arm bodies, and check exhaustiveness.
    pub(crate) fn synth_switch(
        &mut self,
        scrutinee: &Expr,
        arms: &[SwitchArm],
        span: Span,
        expected: Option<TypeId>,
    ) -> TypeId {
        let st = self.synth(scrutinee);

        // Type every arm body, binding the arm's payload capture (for union
        // payloads) to the deferred payload — union-payload typing is comptime in
        // v0.5, so the capture binds to Deferred unless the scrutinee is a union.
        let mut result: Option<TypeId> = None;
        for arm in arms {
            // Bind the arm capture. For a union scrutinee the payload is the
            // matched variant's payload; otherwise (enum/error/int) there is no
            // payload, so a capture binds to the scrutinee's value type.
            self.bind_switch_arm_capture(arm, st);
            // Resolve `.Variant`/`error.Name` pattern items against the scrutinee.
            self.type_arm_items(arm, st);
            // A switch arm body is conditional: a statement-position
            // `@compileError` in it must not fire eagerly.
            self.cond_depth += 1;
            let body_ty = match expected {
                Some(ex) => self.check(&arm.body, ex),
                None => self.synth(&arm.body),
            };
            self.cond_depth -= 1;
            result = Some(match result {
                None => body_ty,
                Some(prev) => self.join(prev, body_ty, arm.span, expected),
            });
        }

        self.check_exhaustive(scrutinee, st, arms, span);

        result.unwrap_or_else(|| self.arena.t_void())
    }

    /// Binds a switch arm's payload capture (if any).
    fn bind_switch_arm_capture(&mut self, arm: &SwitchArm, scrutinee: TypeId) {
        if let Some(cap) = &arm.capture {
            // For a union(enum) scrutinee, look up the matched variant's payload;
            // otherwise the capture binds to the scrutinee value (the error/enum).
            let payload = self.arm_payload_type(arm, scrutinee);
            self.bind_capture(Some(cap), payload);
        }
    }

    /// The payload type captured by an arm. For a concrete union, the matched
    /// variant's payload; otherwise the scrutinee type (or Deferred).
    fn arm_payload_type(&mut self, arm: &SwitchArm, scrutinee: TypeId) -> TypeId {
        if let Type::Union(id) = self.arena.get(scrutinee).clone() {
            if let SwitchPattern::Items(items) = &arm.pattern {
                if let Some(item) = items.first() {
                    if let Some(name) = enum_literal_name(&item.lo) {
                        let info = &self.arena.unions[id.0 as usize];
                        if let Some(v) = info.variants.iter().find(|v| v.name == name) {
                            return v.payload;
                        }
                    }
                }
            }
            return self.arena.t_deferred();
        }
        // Non-union: the capture (rare) binds to the scrutinee type itself.
        scrutinee
    }

    /// Types the pattern items of an arm against the scrutinee type, so a
    /// `.Variant`/`error.Name` item resolves (and a bad one is reported).
    fn type_arm_items(&mut self, arm: &SwitchArm, scrutinee: TypeId) {
        if let SwitchPattern::Items(items) = &arm.pattern {
            for item in items {
                self.type_switch_item(item, scrutinee);
            }
        }
    }

    /// Types one switch item (a value or an inclusive range) against the
    /// scrutinee.
    fn type_switch_item(&mut self, item: &SwitchItem, scrutinee: TypeId) {
        // For an enum/union scrutinee, `.Variant` resolves against it; for an
        // error scrutinee, `error.Name` is just an error literal; otherwise the
        // item is an ordinary value checked against the scrutinee.
        match &item.lo {
            Expr::EnumLiteral { name, span } => {
                let _ = self.check_enum_literal(name, *span, scrutinee);
            }
            Expr::ErrorLiteral { name, span } => {
                let _ = self.synth_error_literal(name, *span);
            }
            other => {
                if !self.arena.is_bottom(scrutinee) {
                    self.check(other, scrutinee);
                } else {
                    self.synth(other);
                }
            }
        }
        if let Some(hi) = &item.hi {
            if !self.arena.is_bottom(scrutinee) {
                self.check(hi, scrutinee);
            } else {
                self.synth(hi);
            }
        }
    }

    /// Checks switch exhaustiveness, duplicate arms, and an unreachable `else`.
    fn check_exhaustive(&mut self, _scrutinee: &Expr, st: TypeId, arms: &[SwitchArm], span: Span) {
        let has_else = arms
            .iter()
            .any(|a| matches!(a.pattern, SwitchPattern::Else));

        match self.arena.get(st).clone() {
            Type::Enum(id) => {
                let all: Vec<String> = self.arena.enums[id.0 as usize]
                    .variants
                    .iter()
                    .map(|v| v.name.clone())
                    .collect();
                let ename = self.arena.enums[id.0 as usize].name.clone();
                self.check_named_exhaustive(&all, arms, has_else, span, &format!("enum `{ename}`"));
            }
            Type::Union(id) => {
                let all: Vec<String> = self.arena.unions[id.0 as usize]
                    .variants
                    .iter()
                    .map(|v| v.name.clone())
                    .collect();
                let uname = self.arena.unions[id.0 as usize].name.clone();
                self.check_named_exhaustive(
                    &all,
                    arms,
                    has_else,
                    span,
                    &format!("union `{uname}`"),
                );
            }
            Type::ErrorSet(id) => {
                let all: Vec<String> = self.arena.errsets[id.0 as usize].members.clone();
                self.check_error_exhaustive(&all, arms, has_else, span);
            }
            Type::Bool => self.check_bool_exhaustive(arms, has_else, span),
            Type::Int { .. } | Type::ComptimeInt => {
                if !has_else {
                    self.error(
                        span,
                        format!("switch on `{}` must have an `else` arm", self.arena.fmt(st)),
                    );
                }
                self.check_duplicate_int_arms(arms);
            }
            // Deferred/anytype/error/other scrutinee: no exhaustiveness check.
            _ => {}
        }
    }

    /// Exhaustiveness over a named variant set (enum/union), keyed by `.Name`
    /// pattern items.
    fn check_named_exhaustive(
        &mut self,
        all: &[String],
        arms: &[SwitchArm],
        has_else: bool,
        span: Span,
        what: &str,
    ) {
        let mut covered: HashSet<String> = HashSet::new();
        let mut dups: Vec<String> = Vec::new();
        for arm in arms {
            if let SwitchPattern::Items(items) = &arm.pattern {
                for item in items {
                    if let Some(name) = enum_literal_name(&item.lo) {
                        if !covered.insert(name.clone()) {
                            dups.push(name);
                        }
                    }
                }
            }
        }
        for d in &dups {
            self.error(span, format!("duplicate switch arm `.{d}`"));
        }
        if has_else {
            // An `else` over an already-fully-covered set is unreachable.
            let missing: Vec<&String> = all.iter().filter(|n| !covered.contains(*n)).collect();
            if missing.is_empty() && !all.is_empty() {
                self.warn(span, "unreachable `else` arm: all cases already covered");
            }
            return;
        }
        let missing: Vec<String> = all
            .iter()
            .filter(|n| !covered.contains(*n))
            .map(|n| format!("`.{n}`"))
            .collect();
        if !missing.is_empty() {
            self.error_rich(
                Diagnostic::error(span, format!("switch on {what} is not exhaustive"))
                    .with_primary_label("this switch does not cover all cases")
                    .with_note(format!("missing cases: {}", missing.join(", ")))
                    .with_help("add the missing arm(s) or an `else =>` branch"),
            );
        }
    }

    /// Exhaustiveness over an error set, keyed by `error.Name` pattern items.
    fn check_error_exhaustive(
        &mut self,
        all: &[String],
        arms: &[SwitchArm],
        has_else: bool,
        span: Span,
    ) {
        let mut covered: HashSet<String> = HashSet::new();
        let mut dups: Vec<String> = Vec::new();
        for arm in arms {
            if let SwitchPattern::Items(items) = &arm.pattern {
                for item in items {
                    if let Some(name) = error_literal_name(&item.lo) {
                        if !covered.insert(name.clone()) {
                            dups.push(name);
                        }
                    }
                }
            }
        }
        for d in &dups {
            self.error(span, format!("duplicate switch arm `error.{d}`"));
        }
        if has_else {
            let missing: Vec<&String> = all.iter().filter(|n| !covered.contains(*n)).collect();
            if missing.is_empty() && !all.is_empty() {
                self.warn(span, "unreachable `else` arm: all cases already covered");
            }
            return;
        }
        let missing: Vec<String> = all
            .iter()
            .filter(|n| !covered.contains(*n))
            .map(|n| format!("`error.{n}`"))
            .collect();
        if !missing.is_empty() {
            self.error_rich(
                Diagnostic::error(span, "switch over error set is not exhaustive")
                    .with_primary_label("this switch does not cover all errors")
                    .with_note(format!("missing cases: {}", missing.join(", ")))
                    .with_help("add the missing arm(s) or an `else =>` branch"),
            );
        }
    }

    /// Exhaustiveness over a `bool` scrutinee: a switch must cover both `true`
    /// and `false`, or carry an `else`. `bool` is a finite, two-element type, so
    /// this mirrors the enum logic over the set `{true, false}` (and warns on an
    /// `else` that is unreachable because both literals are already covered).
    fn check_bool_exhaustive(&mut self, arms: &[SwitchArm], has_else: bool, span: Span) {
        let mut covered_true = false;
        let mut covered_false = false;
        for arm in arms {
            if let SwitchPattern::Items(items) = &arm.pattern {
                for item in items {
                    if item.hi.is_none() {
                        if let Expr::Bool { value, .. } = &item.lo {
                            if *value {
                                covered_true = true;
                            } else {
                                covered_false = true;
                            }
                        }
                    }
                }
            }
        }
        if has_else {
            if covered_true && covered_false {
                self.warn(span, "unreachable `else` arm: all cases already covered");
            }
            return;
        }
        let mut missing: Vec<&str> = Vec::new();
        if !covered_true {
            missing.push("true");
        }
        if !covered_false {
            missing.push("false");
        }
        if !missing.is_empty() {
            self.error(
                span,
                format!(
                    "switch on `bool` is not exhaustive: missing {}",
                    missing.join(", ")
                ),
            );
        }
    }

    /// Flags duplicate integer-literal arms in an integer switch.
    fn check_duplicate_int_arms(&mut self, arms: &[SwitchArm]) {
        let mut seen: HashSet<i128> = HashSet::new();
        for arm in arms {
            if let SwitchPattern::Items(items) = &arm.pattern {
                for item in items {
                    if item.hi.is_none() {
                        if let Expr::Int { text, base, span } = &item.lo {
                            if let Some(v) = crate::eval::parse_int_literal(text, *base) {
                                if !seen.insert(v) {
                                    self.error(
                                        *span,
                                        format!("duplicate switch value `{}`", trimmed(text)),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The `.Name` of an enum-literal pattern item (or a `Type.Variant` field).
fn enum_literal_name(e: &Expr) -> Option<String> {
    match e {
        Expr::EnumLiteral { name, .. } => Some(name.clone()),
        Expr::Field { field, .. } => Some(field.clone()),
        _ => None,
    }
}

/// The `Name` of an `error.Name` pattern item.
fn error_literal_name(e: &Expr) -> Option<String> {
    match e {
        Expr::ErrorLiteral { name, .. } => Some(name.clone()),
        // `error.Name` may also be parsed as a field access on `error`.
        Expr::Field { field, .. } => Some(field.clone()),
        _ => None,
    }
}

/// Trimmed display form of an integer literal lexeme.
fn trimmed(text: &str) -> String {
    text.chars().filter(|c| *c != '_').collect()
}
