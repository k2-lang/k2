//! Positive type-checking tests: one per typing/coercion rule. Each builds a
//! snippet, type-checks it, asserts it is clean, and (where useful) asserts the
//! recorded type at a known span — verifying both that the rule accepts the
//! program and that it infers the right type.

use k2_syntax::Span;
use k2_types::{check_file, Type, TypeId, Typed};

/// Parses, resolves, and type-checks `src`, asserting parse/resolve are clean.
fn check(src: &str) -> Typed {
    let pres = k2_parse::parse(src);
    assert!(pres.is_ok(), "parse: {:#?}", pres.diagnostics);
    let resolved = k2_resolve::resolve_file(&pres.file);
    assert!(
        resolved.is_ok(),
        "resolve: {:#?}",
        resolved.errors().collect::<Vec<_>>()
    );
    check_file(&pres.file, &resolved)
}

/// Asserts a snippet type-checks with zero errors.
fn assert_clean(src: &str) -> Typed {
    let t = check(src);
    assert!(
        t.is_ok(),
        "expected clean type-check, got: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
    t
}

/// The recorded type at the (first, exact) occurrence of `needle` in `src`.
fn type_at(t: &Typed, src: &str, needle: &str) -> TypeId {
    let pos = src.find(needle).expect("needle present") as u32;
    let span = Span::new(pos, pos + needle.len() as u32, 1, 1);
    t.type_at(span)
        .unwrap_or_else(|| panic!("no recorded type at `{needle}`"))
}

#[test]
fn int_literal_coerces_to_sized_int() {
    let src = "fn f() void { const x: u8 = 200; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "200");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(8)
        }
    ));
}

#[test]
fn wide_int_literal() {
    assert_clean("fn f() void { const w: u128 = 1; _ = w; }\n");
}

#[test]
fn comptime_int_arithmetic_stays_comptime() {
    let src = "fn f() void { const x = 1 + 2; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "1 + 2");
    assert!(matches!(t.arena.get(ty), Type::ComptimeInt));
}

#[test]
fn comptime_float_to_f64() {
    assert_clean("fn f() void { const x: f64 = 1.5; _ = x; }\n");
}

#[test]
fn value_to_optional() {
    assert_clean("fn f(x: u32) void { const m: ?u32 = x; _ = m; }\n");
}

#[test]
fn null_to_optional() {
    assert_clean("fn f() void { const n: ?u8 = null; _ = n; }\n");
}

#[test]
fn value_to_error_union_success() {
    assert_clean("fn f() error{A}!u8 { return 1; }\n");
}

#[test]
fn error_to_error_union() {
    assert_clean("fn f() error{A}!u8 { return error.A; }\n");
}

#[test]
fn pointer_adds_const() {
    assert_clean("fn g(p: *const u32) void { _ = p; }\nfn f(p: *u32) void { g(p); }\n");
}

#[test]
fn slice_adds_const() {
    assert_clean("fn g(s: []const u32) void { _ = s; }\nfn f(s: []u32) void { g(s); }\n");
}

#[test]
fn array_pointer_to_slice() {
    // `&arr` yields `*[N]u32`, which coerces to `[]u32`.
    assert_clean(
        "fn g(s: []const u8) void { _ = s; }\nfn f() void { const a = [_]u8{ 1, 2 }; g(a[0..]); }\n",
    );
}

#[test]
fn string_literal_is_const_u8_slice() {
    let src = "fn f() void { const s = \"hi\"; _ = s; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "\"hi\"");
    assert!(matches!(
        t.arena.get(ty),
        Type::Slice { is_const: true, .. }
    ));
}

#[test]
fn try_result_is_ok_payload() {
    // `try g()` in an error-union fn yields g's ok payload `u8`.
    let src =
        "fn g() error{A}!u8 { return 1; }\nfn f() error{A}!void { const x = try g(); _ = x; }\n";
    let t = assert_clean(src);
    // `x` binds to u8.
    let ty = type_at(&t, src, "try g()");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(8)
        }
    ));
}

#[test]
fn catch_result_is_ok_payload() {
    assert_clean(
        "fn g() error{A}!u8 { return 1; }\nfn f() void { const x = g() catch 0; _ = x; }\n",
    );
}

#[test]
fn orelse_result_is_inner() {
    let src = "fn f(o: ?u32) void { const x = o orelse 0; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "o orelse 0");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(32)
        }
    ));
}

#[test]
fn unwrap_result_is_inner() {
    let src = "fn f(o: ?u32) void { const x = o.?; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "o.?");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(32)
        }
    ));
}

#[test]
fn deref_result_is_pointee() {
    let src = "fn f(p: *u32) void { const x = p.*; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "p.*");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(32)
        }
    ));
}

#[test]
fn type_of_builtin_returns_type() {
    let src = "fn f(x: u32) void { const T = @TypeOf(x); _ = T; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "@TypeOf(x)");
    assert!(matches!(t.arena.get(ty), Type::TypeType));
}

#[test]
fn size_of_returns_usize() {
    let src = "fn f() void { const n = @sizeOf(u32); _ = n; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "@sizeOf(u32)");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            bits: k2_types::IntBits::Usize,
            ..
        }
    ));
}

#[test]
fn as_widens_int() {
    assert_clean("fn f(c: u8) void { const w = @as(u32, c); _ = w; }\n");
}

#[test]
fn struct_field_access_type() {
    let src = "const P = struct { x: i32, y: i32 };\nfn f(p: P) void { const a = p.x; _ = a; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "p.x");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: true,
            bits: k2_types::IntBits::Fixed(32)
        }
    ));
}

#[test]
fn method_call_sugar_type() {
    let src = "const P = struct {\n\
               \x20   x: i32,\n\
               \x20   fn get(self: *const P) i32 { return self.x; }\n\
               };\n\
               fn f(p: P) void { const a = p.get(); _ = a; }\n";
    assert_clean(src);
}

#[test]
fn enum_literal_against_enum() {
    let src = "const C = enum { red, green };\nfn f() void { const c: C = .green; _ = c; }\n";
    assert_clean(src);
}

#[test]
fn exhaustive_enum_switch_is_clean() {
    assert_clean(
        "const C = enum { red, green };\nfn f(c: C) u8 { return switch (c) { .red => 0, .green => 1 }; }\n",
    );
}

#[test]
fn exhaustive_error_switch_is_clean() {
    assert_clean(
        "const E = error{ A, B };\nfn f(e: E) u8 { return switch (e) { error.A => 0, error.B => 1 }; }\n",
    );
}

#[test]
fn slice_len_is_usize() {
    let src = "fn f(s: []const u8) void { const n = s.len; _ = n; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "s.len");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            bits: k2_types::IntBits::Usize,
            ..
        }
    ));
}

#[test]
fn index_yields_element() {
    let src = "fn f(s: []u32) void { const x = s[0]; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "s[0]");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: k2_types::IntBits::Fixed(32)
        }
    ));
}

// ---- v0.5 soundness-fix positives (the legit cases must keep passing) ----

#[test]
fn i128_checks_clean() {
    assert_clean("fn f() void { var x: i128 = 0; x = 1; }\n");
    assert_clean(
        "fn f() void { const x: i128 = 170141183460469231731687303715884105727; _ = x; }\n",
    );
    assert_clean(
        "fn f() void { const x: u128 = 170141183460469231731687303715884105728; _ = x; }\n",
    );
}

#[test]
fn try_real_error_union_is_clean() {
    assert_clean(
        "fn g() error{A}!u8 { return 1; }\nfn f() error{A}!void { const x = try g(); _ = x; }\n",
    );
}

#[test]
fn compound_assign_numeric_is_clean() {
    assert_clean("fn f() void { var x: i32 = 0; x += 1; }\n");
}

#[test]
fn write_through_mut_pointer_is_clean() {
    assert_clean("fn f(p: *u32) void { p.* = 5; }\n");
    assert_clean("fn f(s: []u8) void { s[0] = 1; }\n");
}

#[test]
fn as_in_range_literal_is_clean() {
    assert_clean("fn f() void { const x = @as(u8, 5); _ = x; }\n");
    assert_clean("fn f() void { const x = @as(i8, -128); _ = x; }\n");
}

#[test]
fn min_numeric_is_clean() {
    let src = "fn f() void { const z = @min(1, 2); _ = z; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "@min(1, 2)");
    assert!(matches!(t.arena.get(ty), Type::ComptimeInt));
}

#[test]
fn equality_same_enum_is_clean() {
    assert_clean("const A = enum { X, Y };\nfn f(a: A, b: A) bool { return a == b; }\n");
}

#[test]
fn relational_numeric_is_clean() {
    assert_clean("fn f(a: i32, b: i32) bool { return a < b; }\n");
}

#[test]
fn bool_switch_exhaustive_is_clean() {
    assert_clean("fn f(b: bool) u8 { return switch (b) { true => 1, false => 0 }; }\n");
}

#[test]
fn optional_equality_with_null_is_clean() {
    assert_clean("fn f(o: ?i32) bool { return o == null; }\n");
    assert_clean("fn f(o: ?i32) bool { return null == o; }\n");
}

#[test]
fn pointer_equality_same_pointee_is_clean() {
    assert_clean("fn f(a: *i32, b: *const i32) bool { return a == b; }\n");
}

// ---- Deferred-introduction positives -------------------------------------

#[test]
fn generic_call_is_deferred() {
    let src = "fn gen(comptime T: type) T { return undefined; }\nfn f() void { const x = gen(i32); _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "gen(i32)");
    assert!(matches!(t.arena.get(ty), Type::Deferred));
}

#[test]
fn typeinfo_is_deferred() {
    let src = "fn f(comptime T: type) void { const i = @typeInfo(T); _ = i; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "@typeInfo(T)");
    assert!(matches!(t.arena.get(ty), Type::Deferred));
}

#[test]
fn anytype_member_is_deferred() {
    let src = "fn f(out: anytype) void { const x = out.field; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "out.field");
    assert!(matches!(t.arena.get(ty), Type::Deferred));
}

#[test]
fn module_member_is_deferred() {
    let src = "const std = @import(\"std\");\nfn f() void { const x = std.heap; _ = x; }\n";
    let t = assert_clean(src);
    let ty = type_at(&t, src, "std.heap");
    assert!(matches!(t.arena.get(ty), Type::Deferred));
}
