//! Positive name-resolution tests: one per scope / binding rule.
//!
//! Each test parses a snippet (asserting it parses cleanly), resolves it, and
//! asserts both that there are zero resolution errors and that a specific
//! identifier occurrence got the expected [`Resolution`]. Resolutions are looked
//! up by the occurrence's span via the parser's own spans, so we locate an
//! occurrence by finding its text offset in the source.

use k2_resolve::{resolve_file, DefKind, Resolution, Resolved};
use k2_syntax::Span;

/// Parses and resolves `src`, asserting it parses cleanly.
fn resolve(src: &str) -> Resolved {
    let pres = k2_parse::parse(src);
    assert!(
        pres.is_ok(),
        "snippet did not parse cleanly: {:#?}",
        pres.diagnostics
    );
    resolve_file(&pres.file)
}

/// Returns the resolution recorded for the *first* occurrence of `needle` whose
/// byte offset is at or after `from`. We match on the occurrence text + span by
/// scanning the uses table for a use whose name equals `needle` and whose start
/// equals the located scalar offset.
fn res_of_nth(r: &Resolved, src: &str, needle: &str, nth: usize) -> Resolution {
    // Compute scalar (char) offsets of every byte index where `needle` starts,
    // skipping matches inside a double-quoted string literal (so the `std` in
    // `@import("std")` is not mistaken for the identifier reference). Spans use
    // scalar (char) indices, so we track the char position alongside the byte
    // position via `enumerate()`.
    let mut occurrences: Vec<u32> = Vec::new();
    let mut in_string = false;
    let bytes = src.as_bytes();
    let nb = needle.as_bytes();
    for (char_idx, (i, ch)) in src.char_indices().enumerate() {
        if ch == '"' {
            in_string = !in_string;
        }
        if !in_string && bytes[i..].starts_with(nb) {
            // Ensure it is a standalone identifier (not part of a longer word).
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after_i = i + nb.len();
            let after_ok = after_i >= bytes.len() || !is_ident_byte(bytes[after_i]);
            if before_ok && after_ok {
                occurrences.push(char_idx as u32);
            }
        }
    }
    let start = occurrences
        .get(nth)
        .copied()
        .unwrap_or_else(|| panic!("occurrence #{nth} of `{needle}` not found"));
    let span = Span::new(start, start + needle.chars().count() as u32, 0, 0);
    r.uses
        .at(span)
        .unwrap_or_else(|| panic!("no use recorded at `{needle}` (offset {start})"))
        .res
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Asserts a use resolved to a `Def` of the given kind.
fn assert_def_kind(r: &Resolved, res: Resolution, kind: DefKind) {
    match res {
        Resolution::Def(id) => assert_eq!(r.defs[id.0 as usize].kind, kind),
        other => panic!("expected Def({kind:?}), got {other:?}"),
    }
}

#[test]
fn file_scope_forward_reference() {
    // `a` calls `b` declared *after* it — items are order-independent.
    let src = "fn a() void { b(); }\nfn b() void {}\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "b", 0), DefKind::Item);
}

#[test]
fn param_is_visible_in_body_only() {
    let src = "fn f(x: i32) i32 { return x; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // `x` in the body resolves to the param.
    assert_def_kind(&r, res_of_nth(&r, src, "x", 1), DefKind::Param);
}

#[test]
fn block_locals_are_order_dependent_clean() {
    // `const y = x;` where `x` is declared before it: clean.
    let src = "fn f() i32 { const x = 1; const y = x; return y; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "x", 1), DefKind::Local);
}

#[test]
fn container_member_and_self() {
    // A method names a sibling const and `Self`; `Self` is the user's
    // `const Self = @This();` item.
    let src = "\
const C = struct {\n\
    const K = 7;\n\
    const Self = @This();\n\
    pub fn make() Self { return K; }\n\
};\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // `Self` in the return type and `K` in the body both resolve to items.
    assert_def_kind(&r, res_of_nth(&r, src, "K", 1), DefKind::Item);
    assert_def_kind(&r, res_of_nth(&r, src, "Self", 1), DefKind::Item);
}

#[test]
fn field_type_sees_enclosing_generic_param() {
    // The `List(T)` pattern: a struct field's type names the enclosing fn's
    // comptime param `T`.
    let src = "\
fn List(comptime T: type) type {\n\
    return struct {\n\
        items: []T,\n\
    };\n\
}\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "T", 1), DefKind::Param);
}

#[test]
fn if_capture_in_then_only() {
    let src = "fn f(o: ?i32) void { if (o) |x| { _ = x; } }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "x", 1), DefKind::Capture);
}

#[test]
fn for_captures_value_and_index() {
    let src = "fn f(xs: []u32) void { for (xs, 0..) |*slot, i| { _ = slot; _ = i; } }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "slot", 1), DefKind::Capture);
    assert_def_kind(&r, res_of_nth(&r, src, "i", 1), DefKind::Capture);
}

#[test]
fn catch_capture_binds_in_rhs() {
    let src =
        "fn f(r: anyerror!u32) u32 { return r catch |err| blk: { _ = err; break :blk 0; }; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_def_kind(&r, res_of_nth(&r, src, "err", 1), DefKind::Capture);
}

#[test]
fn while_continue_sees_capture() {
    // `while (cond) |c| : (cont) body` — the continuation sees the capture.
    let src =
        "fn f(o: ?u32) void { while (o) |c| : (g(c)) { _ = c; } }\nfn g(x: u32) void { _ = x; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // The `c` inside the continuation `g(c)` resolves to the capture.
    assert_def_kind(&r, res_of_nth(&r, src, "c", 1), DefKind::Capture);
}

#[test]
fn predeclared_types_resolve() {
    let src = "fn f(s: *System, a: Allocator, b: *Build) void { _ = s; _ = a; _ = b; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert!(matches!(
        res_of_nth(&r, src, "System", 0),
        Resolution::Predeclared(_)
    ));
    assert!(matches!(
        res_of_nth(&r, src, "Allocator", 0),
        Resolution::Predeclared(_)
    ));
    assert!(matches!(
        res_of_nth(&r, src, "Build", 0),
        Resolution::Predeclared(_)
    ));
}

#[test]
fn discard_is_not_a_binding() {
    // Multiple `_` params/captures and `_ = expr;` are all fine, no error.
    let src = "fn f(_: i32, _: i32) void { const x = 1; _ = x; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
}

#[test]
fn import_binds_module_and_member_is_deferred() {
    let src = "const std = @import(\"std\");\nfn f() void { const x = std.heap.x; _ = x; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // `std` base resolves to a module; the `.heap.x` members are deferred.
    assert!(matches!(
        res_of_nth(&r, src, "std", 1),
        Resolution::Module(_)
    ));
    assert_eq!(r.modules.len(), 1);
}

#[test]
fn enum_and_error_literals_are_deferred() {
    let src = "fn f() void { const a = .Windows; const b = error.Empty; _ = a; _ = b; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // No error for `.Windows` or `error.Empty`: both are deferred members.
}

#[test]
fn block_label_break_resolves() {
    let src = "fn f() u32 { return blk: { break :blk 1; }; }\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
}

#[test]
fn shadowing_a_predeclared_name_is_allowed() {
    // Per spec §01 5.3 a local may shadow a predeclared type name.
    let src = "fn f() u32 { const u32 = 5; return u32; }\n";
    let r = resolve(src);
    assert!(
        r.is_ok(),
        "shadowing a predeclared name must be allowed: {:#?}",
        r.errors().collect::<Vec<_>>()
    );
}

#[test]
fn arbitrary_width_integers_resolve() {
    // Finding 1: `uN`/`iN` are recognized as primitive types by pattern, not by
    // an enumerated list. The §07 parity-table snippet uses `[256]u1` and
    // `var p: u1 = 0` — both must resolve clean.
    let src = "\
fn flag(b: bool) u1 {\n\
    var table: [256]u1 = undefined;\n\
    var p: u1 = 0;\n\
    _ = table;\n\
    _ = p;\n\
    return if (b) 1 else 0;\n\
}\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // The return-type `u1` resolves as a predeclared primitive, and so does a
    // large but in-bounds width.
    assert!(matches!(
        res_of_nth(&r, src, "u1", 0),
        Resolution::Predeclared(_)
    ));
    let wide = "fn f() void { var x: u65535 = 0; var y: i7 = 0; _ = x; _ = y; }\n";
    let rw = resolve(wide);
    assert!(rw.is_ok(), "{:#?}", rw.errors().collect::<Vec<_>>());
    assert!(matches!(
        res_of_nth(&rw, wide, "u65535", 0),
        Resolution::Predeclared(_)
    ));
}

#[test]
fn anytype_resolves_in_every_type_position() {
    // Finding 2: `anytype` is predeclared, so the bare-identifier spelling
    // resolves in field, return, and var positions (not just params). The §07
    // `Read` struct field is the canonical case.
    let field = "const Read = struct { value: anytype, size: usize };\n";
    let rf = resolve(field);
    assert!(rf.is_ok(), "{:#?}", rf.errors().collect::<Vec<_>>());
    assert!(matches!(
        res_of_nth(&rf, field, "anytype", 0),
        Resolution::Predeclared(_)
    ));

    let ret = "fn f() anytype { return 0; }\n";
    let rr = resolve(ret);
    assert!(rr.is_ok(), "{:#?}", rr.errors().collect::<Vec<_>>());

    let var = "fn f() void { var x: anytype = 0; _ = x; }\n";
    let rv = resolve(var);
    assert!(rv.is_ok(), "{:#?}", rv.errors().collect::<Vec<_>>());
}

#[test]
fn sibling_container_item_reusable_as_param_name() {
    // Finding 3: a method param may reuse a *sibling* container item's name,
    // mirroring the existing field carve-out (`fn at(self, len: i32)` next to
    // `fn len(self)`). Inside `at`, the param `len` wins; `fn len` is reached
    // only qualified.
    let src = "\
const S = struct {\n\
    fn len(self: S) usize { return 0; }\n\
    fn at(self: S, len: i32) i32 { return len; }\n\
};\n";
    let r = resolve(src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    // The `len` referenced in `at`'s body resolves to the param, not the fn.
    assert_def_kind(&r, res_of_nth(&r, src, "len", 2), DefKind::Param);
}

#[test]
fn wide_scope_resolves_in_linear_time() {
    // Finding 4: same-scope duplicate detection is O(1) amortized (HashMap
    // index), so a few thousand distinct file-scope decls resolve quickly rather
    // than quadratically. This is a smoke test of correctness + scale, not a
    // wall-clock benchmark; if duplicate detection were O(N^2) this would still
    // pass but take far longer.
    let n = 5000;
    let mut src = String::with_capacity(n * 16);
    for i in 0..n {
        src.push_str(&format!("const v{i} = {i};\n"));
    }
    let r = resolve(&src);
    assert!(r.is_ok(), "{:#?}", r.errors().collect::<Vec<_>>());
    assert_eq!(r.scopes[r.file_scope.0 as usize].names.len(), n);

    // A duplicate among the many is still detected.
    let dup = format!("{src}const v0 = 999;\n");
    let rd = resolve(&dup);
    assert!(
        rd.errors()
            .any(|e| e.message == "redeclaration of `v0` in this scope"),
        "duplicate must still be detected: {:#?}",
        rd.errors().collect::<Vec<_>>()
    );
}

#[test]
fn import_const_may_share_name_with_fn() {
    // The canonical `build.k2` shape (spec §08 6): `const build = @import("build")`
    // coexists with `pub fn build(b: *Build) void`.
    let src = "const build = @import(\"build\");\npub fn build(b: *Build) void { _ = b; }\n";
    let r = resolve(src);
    assert!(
        r.is_ok(),
        "import/fn name sharing must be allowed: {:#?}",
        r.errors().collect::<Vec<_>>()
    );
}
