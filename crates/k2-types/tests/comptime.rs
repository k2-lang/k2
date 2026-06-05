//! Comptime-engine unit suite (v0.6): proves the comptime evaluator, generic
//! instantiation, and reflection *actually evaluate* — not blanket-defer.
//!
//! Each test type-checks a k2 snippet and asserts a recorded type at a known
//! span is now *concrete* (a sized int, a struct, a usize with a known value)
//! where v0.5 produced `Deferred`. The instantiation-identity tests assert that
//! two calls with the same arguments produce the *same* `TypeId`, and two with
//! different arguments produce *distinct* ones — the monomorphization-cache
//! guarantee (spec §07.4.3).

use k2_syntax::Span;
use k2_types::{check_file, IntBits, Type, TypeId, Typed};

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

/// Type-checks `src`, asserting zero error diagnostics.
fn check_clean(src: &str) -> Typed {
    let t = check(src);
    assert!(
        t.is_ok(),
        "expected clean, got: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
    t
}

/// The recorded type at the first exact occurrence of `needle`.
fn type_at(t: &Typed, src: &str, needle: &str) -> TypeId {
    let pos = src.find(needle).expect("needle present") as u32;
    let span = Span::new(pos, pos + needle.len() as u32, 1, 1);
    t.type_at(span)
        .unwrap_or_else(|| panic!("no recorded type at `{needle}`"))
}

/// The recorded type at the `n`th (0-based) occurrence of `needle`.
fn type_at_nth(t: &Typed, src: &str, needle: &str, n: usize) -> TypeId {
    let mut start = 0usize;
    let mut found = None;
    for _ in 0..=n {
        let rel = src[start..].find(needle).expect("enough occurrences");
        let pos = start + rel;
        found = Some(pos);
        start = pos + needle.len();
    }
    let pos = found.unwrap() as u32;
    let span = Span::new(pos, pos + needle.len() as u32, 1, 1);
    t.type_at(span)
        .unwrap_or_else(|| panic!("no recorded type at occurrence {n} of `{needle}`"))
}

// =========================================================================
//  1. Arithmetic / control flow inside a comptime const fold.
// =========================================================================

/// The binding type of `var x`/`const x` is recorded at its read site `_ = x`.
/// This helper reads the type at the trailing `= x` occurrence (the discard
/// read), which synthesizes to the binding's declared type.
fn read_type(t: &Typed, src: &str, var: &str) -> TypeId {
    let needle = format!("= {var};");
    let pos = src.find(&needle).expect("read site present") as u32 + 2;
    let span = Span::new(pos, pos + var.len() as u32, 1, 1);
    t.type_at(span)
        .unwrap_or_else(|| panic!("no recorded type at read of `{var}`"))
}

#[test]
fn comptime_sizeof_is_concrete_usize_value() {
    // `@sizeOf(u32)` used as an array length resolves to a concrete `[4]u8`.
    let src = "fn f() void { var b: [@sizeOf(u32)]u8 = undefined; _ = b; }\n";
    let t = check_clean(src);
    let ty = read_type(&t, src, "b");
    match t.arena.get(ty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(4)),
        other => panic!("expected [4]u8, got {other:?}"),
    }
}

#[test]
fn sizeof_scalars_drive_array_lengths() {
    // u8->1, bool->1, u64->8, [4]u16->8, *u8->8, []u8->16.
    for (ty_name, expect) in [
        ("u8", 1u64),
        ("bool", 1),
        ("u64", 8),
        ("[4]u16", 8),
        ("*u8", 8),
        ("[]u8", 16),
        ("f64", 8),
        ("u128", 16),
    ] {
        let src = format!("fn f() void {{ var b: [@sizeOf({ty_name})]u8 = undefined; _ = b; }}\n");
        let t = check_clean(&src);
        let ty = read_type(&t, &src, "b");
        match t.arena.get(ty) {
            Type::Array { len, .. } => assert_eq!(
                *len,
                k2_types::ArrayLen::Known(expect),
                "@sizeOf({ty_name}) should be {expect}"
            ),
            other => panic!("@sizeOf({ty_name}): expected array, got {other:?}"),
        }
    }
}

#[test]
fn struct_layout_padding_is_correct() {
    // struct{a:u8,b:u32,c:u8}: offsets 0,4,8 -> size rounds to align 4 = 12.
    let src = "const S = struct { a: u8, b: u32, c: u8 };\nfn f() void { var z: [@sizeOf(S)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    let ty = read_type(&t, src, "z");
    match t.arena.get(ty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(12)),
        other => panic!("expected [12]u8, got {other:?}"),
    }
}

#[test]
fn comptime_shift_folds() {
    // A comptime fold used as an array length.
    let src = "fn f() void { const n = 1 << 10; var z: [n]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    let ty = read_type(&t, src, "z");
    match t.arena.get(ty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(1024)),
        other => panic!("expected [1024]u8, got {other:?}"),
    }
}

// =========================================================================
//  2. Generic instantiation + caching identity (spec §07.4.3).
// =========================================================================

const LIST: &str = "fn List(comptime T: type) type {\n\
    return struct {\n\
        items: []T,\n\
        len: usize,\n\
        pub fn first(self: *Self) T { return self.items[0]; }\n\
        const Self = @This();\n\
    };\n\
}\n";

#[test]
fn same_generic_args_share_one_type() {
    let src = format!(
        "{LIST}fn f() void {{ var a: List(u32) = undefined; var b: List(u32) = undefined; _ = a; _ = b; }}\n"
    );
    let t = check_clean(&src);
    let a = type_at_nth(&t, &src, "List(u32)", 0);
    let b = type_at_nth(&t, &src, "List(u32)", 1);
    assert_eq!(a, b, "List(u32) twice must be the SAME interned type");
    assert!(matches!(t.arena.get(a), Type::Struct(_)));
}

#[test]
fn different_generic_args_are_distinct_types() {
    let src = format!(
        "{LIST}fn f() void {{ var a: List(u32) = undefined; var b: List(u8) = undefined; _ = a; _ = b; }}\n"
    );
    let t = check_clean(&src);
    let a = type_at(&t, &src, "List(u32)");
    let b = type_at(&t, &src, "List(u8)");
    assert_ne!(a, b, "List(u32) and List(u8) must be DISTINCT types");
}

#[test]
fn generic_value_param_distinct_per_value() {
    let src = "fn Buffer(comptime n: usize) type { return struct { data: [n]u8 }; }\n\
        fn f() void { var a: Buffer(64) = undefined; var b: Buffer(64) = undefined; var c: Buffer(8) = undefined; _ = a; _ = b; _ = c; }\n";
    let t = check_clean(src);
    let a = type_at(&t, src, "Buffer(64)");
    let b = type_at_nth(&t, src, "Buffer(64)", 1);
    let c = type_at(&t, src, "Buffer(8)");
    assert_eq!(a, b, "Buffer(64) twice is one type");
    assert_ne!(a, c, "Buffer(64) and Buffer(8) are distinct");
}

#[test]
fn generic_function_instantiates_to_return_type() {
    // `maxOf(i32, a, b)` returns `T` = i32 concretely.
    let src = "fn maxOf(comptime T: type, a: T, b: T) T { return if (a > b) a else b; }\n\
        fn f() void { const m = maxOf(i32, 3, 9); _ = m; }\n";
    let t = check_clean(src);
    let ty = type_at(&t, src, "maxOf(i32, 3, 9)");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: true,
            bits: IntBits::Fixed(32)
        }
    ));
}

// =========================================================================
//  3. @typeInfo / @Type round-trip.
// =========================================================================

#[test]
fn type_round_trip_primitive_is_identity() {
    // `@Type(@typeInfo(u32))` must intern to the SAME TypeId as `u32` — the
    // spec's `@Type(@typeInfo(T)) == T` law (the arena interns u32 once). We bind
    // `var a: id(u32)` and `var b: u32` and compare their recorded read types.
    let src = "fn id(comptime T: type) type { return @Type(@typeInfo(T)); }\n\
        fn f() void { var a: id(u32) = undefined; var b: u32 = undefined; _ = a; _ = b; }\n";
    let t = check_clean(src);
    let ra = read_type(&t, src, "a");
    let rb = read_type(&t, src, "b");
    assert_eq!(
        ra, rb,
        "@Type(@typeInfo(u32)) must be the SAME interned TypeId as u32"
    );
    assert!(matches!(
        t.arena.get(ra),
        Type::Int {
            signed: false,
            bits: IntBits::Fixed(32)
        }
    ));
}

#[test]
fn uint_from_width_builds_sized_int() {
    let src = "fn UInt(comptime bits: u16) type { return @Type(.{ .Int = .{ .signedness = .unsigned, .bits = bits } }); }\n\
        fn f() void { var x: UInt(24) = undefined; _ = x; }\n";
    let t = check_clean(src);
    let ty = type_at(&t, src, "UInt(24)");
    assert!(matches!(
        t.arena.get(ty),
        Type::Int {
            signed: false,
            bits: IntBits::Fixed(24)
        }
    ));
}

#[test]
fn optionalize_round_trips_through_type() {
    let src = "fn Opt(comptime T: type) type { return @Type(@typeInfo(?T)); }\n\
        fn f() void { var x: Opt(u8) = undefined; _ = x; }\n";
    let t = check_clean(src);
    let ty = type_at(&t, src, "Opt(u8)");
    match t.arena.get(ty) {
        Type::Optional(inner) => assert!(matches!(
            t.arena.get(*inner),
            Type::Int {
                signed: false,
                bits: IntBits::Fixed(8)
            }
        )),
        other => panic!("expected ?u8, got {other:?}"),
    }
}

// =========================================================================
//  4. @hasField over a concrete struct.
// =========================================================================

#[test]
fn hasfield_decides_at_comptime() {
    let src = "const P = struct { x: i32, y: i32 };\n\
        fn has(comptime T: type) bool { return @hasField(T, \"x\"); }\n\
        fn f() void { const a = has(P); _ = a; }\n";
    // The point is only that it type-checks clean (a bool is produced, the
    // instantiation runs `@hasField` concretely).
    check_clean(src);
}

// =========================================================================
//  5. inline for unrolling over a comptime field list.
// =========================================================================

#[test]
fn inline_for_over_fields_type_checks() {
    // The reflection shape from comptime_reflection.k2, minimized.
    let src = "const P = struct { a: u16, b: u32 };\n\
        fn total(comptime T: type) usize {\n\
            var n: usize = 0;\n\
            inline for (@typeInfo(T).Struct.fields) |field| {\n\
                n += @sizeOf(field.type);\n\
            }\n\
            return n;\n\
        }\n\
        fn f() void { var z: [total(P)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    // u16 + u32 = 6.
    let ty = read_type(&t, src, "z");
    match t.arena.get(ty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(6)),
        other => panic!("expected [6]u8, got {other:?}"),
    }
}

// =========================================================================
//  6. v0.6 defect regressions.
// =========================================================================

/// `true` if some error diagnostic contains `needle`.
fn has_error_with(t: &Typed, needle: &str) -> bool {
    t.errors().any(|d| d.message.contains(needle))
}

/// The recorded array length at a `var z: [..]u8` read site (used to observe a
/// comptime-folded `@bitSizeOf`/`@sizeOf` value through the array length).
fn z_len(t: &Typed, src: &str) -> k2_types::ArrayLen {
    let ty = read_type(t, src, "z");
    match t.arena.get(ty) {
        Type::Array { len, .. } => *len,
        other => panic!("expected an array type, got {other:?}"),
    }
}

// ---- [BLOCKER] arity guards: under-applied reflection builtins diagnose,
//      never panic (spec §07.10). --------------------------------------------

#[test]
fn under_applied_reflection_builtins_diagnose_not_panic() {
    // Each of these used to index `args[..]` out of bounds and PANIC the whole
    // compiler. They must now each produce an error diagnostic and return.
    for src in [
        "const S = struct { x: i32 };\nconst R = @Type();\n",
        "const S = struct { x: i32 };\nconst R = @hasField(S);\n",
        "const S = struct { x: i32 };\nconst R = @field(S);\n",
        "const S = struct { x: i32 };\nconst R = @sizeOf();\n",
    ] {
        let t = check(src);
        assert!(
            !t.is_ok(),
            "under-applied builtin should diagnose, not pass: {src}"
        );
        assert!(
            has_error_with(&t, "expects"),
            "expected an arity diagnostic for: {src}\n{:#?}",
            t.errors().collect::<Vec<_>>()
        );
    }
}

// ---- [BLOCKER] @Type/@typeInfo pointer round-trip honors .One vs .Slice. ----

#[test]
fn pointer_round_trip_is_single_item_pointer() {
    // `@Type(@typeInfo(*u8))` must be `*u8` (size 8), NOT a slice (size 16).
    let src = "fn id(comptime T: type) type { return @Type(@typeInfo(T)); }\n\
        fn f(p: id(*u8)) void { _ = p; }\n";
    let t = check_clean(src);
    let pos = src.find("p: id(*u8)").unwrap() as u32 + 3;
    let span = Span::new(pos, pos + "id(*u8)".len() as u32, 1, 1);
    let ty = t.type_at(span).expect("recorded type at id(*u8)");
    match t.arena.get(ty) {
        Type::Pointer { pointee, .. } => assert!(matches!(
            t.arena.get(*pointee),
            Type::Int {
                signed: false,
                bits: IntBits::Fixed(8)
            }
        )),
        other => panic!("expected *u8, got {other:?}"),
    }
}

#[test]
fn slice_round_trip_stays_slice() {
    let src = "fn id(comptime T: type) type { return @Type(@typeInfo(T)); }\n\
        fn f(p: id([]u8)) void { _ = p; }\n";
    let t = check_clean(src);
    let pos = src.find("p: id([]u8)").unwrap() as u32 + 3;
    let span = Span::new(pos, pos + "id([]u8)".len() as u32, 1, 1);
    let ty = t.type_at(span).expect("recorded type at id([]u8)");
    assert!(
        matches!(t.arena.get(ty), Type::Slice { .. }),
        "expected []u8"
    );
}

// ---- [BLOCKER] type-value equality (`T == U`, spec §07.4.2). ----------------

#[test]
fn type_value_equality_yields_bool_no_error() {
    // The verbatim spec example: a type-returning generic call result compares
    // by type identity against a type literal, with no "cannot compare" error.
    let src = "fn id(comptime T: type) type { return T; }\n\
        const b: bool = id(u32) == u32;\n\
        fn f() void { _ = b; }\n";
    check_clean(src);
}

#[test]
fn generic_type_identity_equality() {
    let src = "fn List(comptime T: type) type { return struct { items: []T }; }\n\
        const a: bool = List(u32) == List(u32);\n\
        const b: bool = List(u32) == List(u8);\n\
        const c: bool = id(u32) == id(i32);\n\
        fn id(comptime T: type) type { return T; }\n\
        fn f() void { _ = a; _ = b; _ = c; }\n";
    check_clean(src);
}

#[test]
fn value_versus_type_comparison_still_errors() {
    // Soundness: comparing a real value with a type must STILL be rejected — the
    // type-eq fix must not open a hole.
    let t = check("fn f(x: u32) void { const b = x == u32; _ = b; }\n");
    assert!(
        has_error_with(&t, "cannot compare"),
        "value-vs-type comparison must error: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

// ---- [MAJOR] @compileError fires in every executed position. ----------------

#[test]
fn top_level_compile_error_fires() {
    let t = check("const x = @compileError(\"boom\");\n");
    assert!(
        has_error_with(&t, "boom"),
        "top-level @compileError must fire"
    );
}

#[test]
fn statement_compile_error_fires() {
    let t = check("fn f() void { @compileError(\"boom\"); }\n");
    assert!(
        has_error_with(&t, "boom"),
        "statement @compileError must fire"
    );
}

#[test]
fn dead_branch_compile_error_does_not_fire() {
    // A `@compileError` in a statically-dead branch must stay silent.
    let t = check("fn f(c: bool) void { if (false) { @compileError(\"dead\"); } _ = c; }\n");
    assert!(
        !has_error_with(&t, "dead"),
        "dead-branch @compileError must NOT fire: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

// ---- [MAJOR] comptime termination: exponential concat is bounded. -----------

#[test]
fn exponential_concat_terminates_with_diagnostic() {
    // `s = s ++ s` doubling each iteration must be stopped by the fuel/size
    // budget with a diagnostic — never hang or OOM.
    let src = "const X = comptime blk: { var s: []const u8 = \"aaaaaaaaaaaaaaaa\"; \
        var i: usize = 0; while (i < 40) : (i += 1) { s = s ++ s; } break :blk s; };\n\
        fn f() void { _ = X; }\n";
    let t = check(src);
    assert!(
        has_error_with(&t, "exceeded") || has_error_with(&t, "size limit"),
        "exponential concat must terminate with a budget diagnostic: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

// ---- [MAJOR] array length overflow is rejected, not truncated. --------------

#[test]
fn oversized_array_length_is_rejected_not_truncated() {
    // `[(1 << 64) + 7]u8` must NOT silently become `[7]u8`.
    let t = check("const a: [(1 << 64) + 7]u8 = undefined;\nfn f() void { _ = a; }\n");
    assert!(
        has_error_with(&t, "array length too large"),
        "oversized array length must be rejected: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

// ---- [MAJOR] @bitSizeOf is the true bit width. ------------------------------

#[test]
fn bit_size_of_is_true_bit_width() {
    for (ty_name, bits) in [
        ("u7", 7u64),
        ("bool", 1),
        ("u24", 24),
        ("u32", 32),
        ("u1", 1),
    ] {
        let src =
            format!("fn f() void {{ var z: [@bitSizeOf({ty_name})]u8 = undefined; _ = z; }}\n");
        let t = check_clean(&src);
        assert_eq!(
            z_len(&t, &src),
            k2_types::ArrayLen::Known(bits),
            "@bitSizeOf({ty_name}) should be {bits}"
        );
    }
}

// ---- [MAJOR] generic instantiation soundness: ill-typed-for-T is reported. --

#[test]
fn ill_typed_instantiated_method_is_reported() {
    // `self.val * self.val` is a type error when `T = bool`; it must be reported
    // with the per-instantiation concrete type.
    let src = "fn Box(comptime T: type) type {\n\
        return struct {\n\
            val: T,\n\
            const Self = @This();\n\
            fn bad(self: *Self) void { const x = self.val * self.val; _ = x; }\n\
        };\n\
    }\n\
    fn f(b: Box(bool)) void { _ = b; }\n";
    let t = check(src);
    assert!(
        has_error_with(&t, "incompatible types `bool` and `bool`"),
        "ill-typed instantiated method must be reported: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

#[test]
fn well_typed_instantiated_method_is_clean() {
    // The same generic with a `T` for which the body IS well-typed stays clean.
    let src = "fn Box(comptime T: type) type {\n\
        return struct {\n\
            val: T,\n\
            const Self = @This();\n\
            fn good(self: *Self) T { return self.val; }\n\
        };\n\
    }\n\
    fn f(b: Box(u32)) void { _ = b; }\n";
    check_clean(src);
}

// ---- [MINOR] @Type integer descriptor range-checks .bits. -------------------

#[test]
fn type_int_descriptor_rejects_out_of_range_bits() {
    let src = "fn UInt(comptime bits: i32) type { \
        return @Type(.{ .Int = .{ .signedness = .unsigned, .bits = bits } }); }\n\
        fn f(x: UInt(-5)) void { _ = x; }\n";
    let t = check(src);
    assert!(
        has_error_with(&t, "out of range"),
        "negative @Type bit width must be rejected: {:#?}",
        t.errors().collect::<Vec<_>>()
    );
}

// ---- [MINOR] inferred-tag enum layout is minimal. ---------------------------

#[test]
fn inferred_tag_enum_layout_is_minimal() {
    // A 3-variant enum fits a 1-byte tag, so `@sizeOf` is 1, not 8.
    let src = "const E = enum { a, b, c };\n\
        fn f() void { var z: [@sizeOf(E)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(1));
}

// =========================================================================
//  v0.21: packed-struct / align(N) / @Vector layout facts.
// =========================================================================

#[test]
fn packed_struct_size_and_bits() {
    // packed struct { a:u3, b:u5 } -> 8 bits -> 1 byte.
    let src = "const F = packed struct { a: u3, b: u5 };\n\
        fn f() void { var z: [@sizeOf(F)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(1));

    // packed struct { a:u3, b:u3, c:u3 } -> 9 bits -> 2 bytes; bitSize == 9.
    let src2 = "const F = packed struct { a: u3, b: u3, c: u3 };\n\
        fn f() void { var z: [@sizeOf(F)]u8 = undefined; _ = z; \
                      var w: [@bitSizeOf(F)]u8 = undefined; _ = w; }\n";
    let t2 = check_clean(src2);
    assert_eq!(z_len(&t2, src2), k2_types::ArrayLen::Known(2));
    let wty = read_type(&t2, src2, "w");
    match t2.arena.get(wty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(9)),
        other => panic!("expected [9]u8 for @bitSizeOf, got {other:?}"),
    }
}

#[test]
fn packed_struct_byte_fields_size() {
    // packed struct { a:u8, b:u16, c:u8 } -> 32 bits -> 4 bytes.
    let src = "const F = packed struct { a: u8, b: u16, c: u8 };\n\
        fn f() void { var z: [@sizeOf(F)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(4));
}

#[test]
fn align_raises_struct_size() {
    // struct { a:u8, b:u32 align(8) } -> b at offset 8, size rounds to 16.
    let src = "const S = struct { a: u8, b: u32 align(8) };\n\
        fn f() void { var z: [@sizeOf(S)]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(16));
}

#[test]
fn align_raises_alignof_and_offsetof() {
    let src = "const S = struct { a: u8, b: u32 align(8) };\n\
        fn f() void { var z: [@alignOf(S)]u8 = undefined; _ = z; \
                      var w: [@offsetOf(S, \"b\")]u8 = undefined; _ = w; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(8));
    let wty = read_type(&t, src, "w");
    match t.arena.get(wty) {
        Type::Array { len, .. } => assert_eq!(*len, k2_types::ArrayLen::Known(8)),
        other => panic!("expected [8]u8 for @offsetOf, got {other:?}"),
    }
}

#[test]
fn vector_size_and_align() {
    // @sizeOf(@Vector(4, u32)) == 16; @alignOf == 16.
    let src = "fn f() void { var z: [@sizeOf(@Vector(4, u32))]u8 = undefined; _ = z; }\n";
    let t = check_clean(src);
    assert_eq!(z_len(&t, src), k2_types::ArrayLen::Known(16));

    let src2 = "fn f() void { var z: [@alignOf(@Vector(4, u32))]u8 = undefined; _ = z; }\n";
    let t2 = check_clean(src2);
    assert_eq!(z_len(&t2, src2), k2_types::ArrayLen::Known(16));
}
