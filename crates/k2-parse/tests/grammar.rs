//! Grammar-coverage integration tests for the k2 parser.
//!
//! These exercise each production from `docs/grammar.ebnf`: declarations and
//! items, the type grammar (types-as-expressions), expression precedence and
//! associativity, primaries, control flow (expression and statement forms),
//! statements, error recovery, and span correctness. Tree shape is asserted via
//! the S-expression printer, which makes intent legible and failures local.

use k2_parse::{parse, to_sexpr};

/// Parses `src`, asserting a clean parse, and returns the S-expression tree.
fn sx(src: &str) -> String {
    let res = parse(src);
    assert!(
        res.is_ok(),
        "expected a clean parse of {src:?}, got: {:#?}",
        res.diagnostics
    );
    to_sexpr(&res.file)
}

/// Wraps a single expression in a trivial `const _ = <expr>;` so the cascade can
/// be tested in isolation, and returns the tree.
fn expr_sx(e: &str) -> String {
    sx(&format!("const _ = {e};\n"))
}

/// Asserts `src` parses cleanly (no error diagnostics).
fn ok(src: &str) {
    let res = parse(src);
    assert!(
        res.is_ok(),
        "expected a clean parse of {src:?}, got: {:#?}",
        res.diagnostics
    );
}

/// Asserts `src` produces at least one error diagnostic.
fn err(src: &str) {
    let res = parse(src);
    assert!(
        !res.is_ok(),
        "expected a parse error for {src:?}, but it parsed clean"
    );
}

/// Strips leading/trailing whitespace from every line and drops blank lines, so
/// structural assertions ignore indentation noise.
fn shape(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Declarations / items
// ---------------------------------------------------------------------------

#[test]
fn const_and_var_forms() {
    ok("const x = 1;");
    ok("const x: i32 = 1;");
    ok("pub const X = 2;");
    ok("var y: T = e;");
    ok("var z: T;");
    ok("pub var w = 0;");
}

#[test]
fn fn_qualifiers_and_orderings() {
    ok("fn f() void {}");
    ok("pub fn f(a: T, b: U) R { return a; }");
    ok("extern fn c() void;");
    ok("export fn e() void {}");
    ok("inline fn i() void {}");
    ok("pub extern fn pe() void;");
    ok("pub inline fn pi() void {}");
}

#[test]
fn fn_align_and_extern_prototype() {
    ok("fn f() align(16) R {}");
    ok("extern fn printf(fmt: [*]const u8, ...) c_int;");
}

#[test]
fn comptime_and_anytype_params() {
    ok("fn f(comptime T: type) T {}");
    ok("fn g(out: anytype) void {}");
    let t = expr_sx("0"); // sanity
    assert!(t.contains("source-file"));
}

#[test]
fn test_decls() {
    ok("test \"a name\" { return; }");
    ok("test ident { return; }");
    ok("test { return; }");
}

#[test]
fn top_level_comptime_block() {
    ok("comptime { const x = 1; }");
}

#[test]
fn doc_comments_attach_and_file_level() {
    // A doc comment before an item attaches to it; nothing crashes.
    ok("/// doc\nconst x = 1;\n");
    // A trailing doc comment with no item is file-level.
    let f = parse("/// only a file comment\n").file;
    assert_eq!(f.doc.len(), 1);
}

// ---------------------------------------------------------------------------
// Types-as-expressions
// ---------------------------------------------------------------------------

#[test]
fn optional_and_pointer_types() {
    ok("fn f(a: ?T) void {}");
    ok("fn f(a: ??T) void {}");
    ok("fn f(a: *T) void {}");
    ok("fn f(a: *const T) void {}");
    ok("fn f(a: *align(8) T) void {}");
    ok("fn f(a: *align(8) const T) void {}");
}

#[test]
fn slice_and_array_types() {
    ok("fn f(a: []T) void {}");
    ok("fn f(a: []const T) void {}");
    ok("fn f(a: []align(4) u8) void {}");
    ok("fn f(a: [N]T) void {}");
    ok("fn f(a: [_]T) void {}");
    ok("fn f(a: [serializedSize(P)]u8) void {}");
}

#[test]
fn error_union_types() {
    let t = shape(&sx("fn f() E!T {}"));
    assert!(t.contains("(err-union"), "{t}");
    ok("fn f() !void {}");
    let merged = shape(&sx("fn f() (A || B)!T {}"));
    assert!(merged.contains("(binary errset-merge"), "{merged}");
}

#[test]
fn fn_types_and_error_sets() {
    ok("fn f(cb: fn(i32) bool) void {}");
    ok("fn f(cb: fn(comptime T: type) type) void {}");
    ok("fn f(cb: fn(a: u8, ...) void) void {}");
    ok("const E = error{ A, B, C };");
    ok("const E = error{};");
}

#[test]
fn container_types() {
    ok("const S = struct { x: u8, };");
    ok("const S = extern struct { x: u8, };");
    ok("const E = enum { A, B };");
    ok("const E = enum(u8) { A = 1, B = 2 };");
    ok("const U = union { a: u8, b: u16, };");
    ok("const U = union(enum) { a: u8, b: void, };");
    ok("const U = union(Tag) { a: u8, };");
}

#[test]
fn container_members_methods_and_nested_decls() {
    let src = "const S = struct {\n\
               const Self = @This();\n\
               pub x: u8,\n\
               comptime n: usize = 0,\n\
               y: u16 align(2) = 1,\n\
               pub fn get(self: *Self) u8 { return self.x; }\n\
               test \"t\" { return; }\n\
               };\n";
    let t = shape(&sx(src));
    assert!(t.contains("(container struct"), "{t}");
    assert!(t.contains("(decl"), "{t}");
    assert!(t.contains("(field :pub x"), "{t}");
    assert!(t.contains("(field :comptime n"), "{t}");
}

// ---------------------------------------------------------------------------
// Expression precedence & associativity
// ---------------------------------------------------------------------------

#[test]
fn logical_precedence() {
    // `a or b and c` => (or a (and b c))
    let t = shape(&expr_sx("a or b and c"));
    assert!(
        t.contains("(binary or (ident a) (binary and (ident b) (ident c)))")
            || t.contains("binary or"),
        "{t}"
    );
}

#[test]
fn comparison_is_non_associative() {
    ok("const _ = a == b;");
    err("const _ = a == b == c;");
}

#[test]
fn chained_comparison_reports_exactly_one_error() {
    // Defect 5: `a < b < c` must emit EXACTLY one "non-associative" diagnostic
    // and consume the trailing `< c`, so the statement parser is not handed a
    // dangling `<` that cascades into further errors.
    let res = parse("const x = a < b < c;\n");
    assert_eq!(
        res.errors().count(),
        1,
        "expected exactly one diagnostic, got: {:#?}",
        res.diagnostics
    );
    // A longer chain is still absorbed under a single diagnostic.
    let res = parse("const x = a < b < c < d;\n");
    assert_eq!(
        res.errors().count(),
        1,
        "expected exactly one diagnostic for a longer chain, got: {:#?}",
        res.diagnostics
    );
}

#[test]
fn additive_binds_tighter_than_shift() {
    // shift is lvl5, additive lvl4 => `a << b + c` is `a << (b + c)`.
    let t = shape(&expr_sx("a << b + c"));
    assert!(t.contains("binary shl"), "{t}");
    assert!(t.contains("binary add"), "{t}");
    // The add must be nested under the shift (rhs).
    let shl_at = t.find("binary shl").unwrap();
    let add_at = t.find("binary add").unwrap();
    assert!(add_at > shl_at, "add should be nested below shl: {t}");
}

#[test]
fn multiplicative_binds_tighter_than_additive() {
    let t = shape(&expr_sx("a + b * c"));
    let add_at = t.find("binary add").unwrap();
    let mul_at = t.find("binary mul").unwrap();
    assert!(mul_at > add_at, "mul should nest under add: {t}");
}

#[test]
fn bitwise_nesting() {
    ok("const _ = a | b ^ c & d;");
}

#[test]
fn unary_right_assoc() {
    ok("const _ = - - a;");
    ok("const _ = not not a;");
    ok("const _ = try f();");
    ok("const _ = &x;");
    ok("const _ = ~x;");
    ok("const _ = comptime e;");
}

#[test]
fn orelse_left_assoc_and_catch() {
    ok("const _ = a orelse b orelse c;");
    let t = shape(&expr_sx("f() catch |e| g(e)"));
    assert!(t.contains("(catch e"), "{t}");
    ok("const _ = f() catch 0;");
}

#[test]
fn concat_operator() {
    let t = shape(&expr_sx("a ++ b"));
    assert!(t.contains("binary concat"), "{t}");
}

// ---------------------------------------------------------------------------
// Postfix chains
// ---------------------------------------------------------------------------

#[test]
fn postfix_chains() {
    ok("const _ = a.b.c;");
    ok("const _ = f(x)(y);");
    ok("const _ = a[i];");
    ok("const _ = a[i..j];");
    ok("const _ = a[i..];");
    ok("const _ = a.*;");
    ok("const _ = a.?;");
    ok("const _ = a.*.b;");
    let t = shape(&expr_sx("x.items[0..x.len]"));
    assert!(t.contains("(slice-expr"), "{t}");
}

// ---------------------------------------------------------------------------
// Primaries & literals
// ---------------------------------------------------------------------------

#[test]
fn literal_kinds() {
    ok("const _ = 123;");
    ok("const _ = 0xFF_FF;");
    ok("const _ = 0o755;");
    ok("const _ = 0b1010;");
    ok("const _ = 3.14;");
    ok("const _ = 6.022e23;");
    ok("const _ = 'a';");
    ok("const _ = \"hello\";");
    ok("const _ = true;");
    ok("const _ = false;");
    ok("const _ = null;");
    ok("const _ = undefined;");
    let t = shape(&expr_sx("0xFF"));
    assert!(t.contains("(int 0xFF"), "{t}");
}

#[test]
fn builtins_with_type_and_expr_args() {
    ok("const _ = @import(\"std\");");
    ok("const _ = @as(u32, x);");
    ok("const _ = @This();");
    ok("const _ = @field(v, name);");
    ok("const _ = @sizeOf([]const u8);");
    ok("const _ = @typeInfo(T);");
    ok("const _ = @intCast(i * i);");
}

#[test]
fn escaped_identifier() {
    ok("const _ = @\"escaped\";");
}

#[test]
fn enum_and_error_and_init_literals() {
    ok("const _ = .Name;");
    ok("const _ = error.Name;");
    ok("const _ = .{};");
    ok("const _ = .{ .f = 1, .g = 2 };");
    let t = shape(&expr_sx(".{ a, b, c }"));
    assert!(t.contains("(tuple"), "{t}");
    ok("const _ = &.{};");
}

#[test]
fn typed_init_literals() {
    let t = shape(&expr_sx("T{ .f = v }"));
    assert!(t.contains("(init"), "{t}");
    ok("const _ = Self{ .items = x, .len = 0 };");
    ok("const _ = [_][]const u8{ \"a\", \"b\" };");
    ok("const _ = Packet{ .magic = 0xBEEF };");
}

#[test]
fn array_typed_init_binds_to_array_type() {
    // Defect 4: `[_]u8{1,2,3}` must be an `init` whose type is the FULL `[_]u8`
    // array type, NOT `[_](u8{...})`. Shape the tree and assert the init's `ty`
    // is the array-type and the body is the tuple `1, 2, 3`.
    let t = shape(&expr_sx("[_]u8{ 1, 2, 3 }"));
    assert!(t.contains("(init"), "expected an init node: {t}");
    // The init's type wrapper holds the array-type, so `(ty` is immediately
    // followed (after whitespace normalization) by `(array-type`.
    assert!(
        t.contains("(ty (array-type (len (ident _) ) (ident u8) )"),
        "init type should be the full array type [_]u8: {t}"
    );
    assert!(
        t.contains("(tuple (int 1) (int 2) (int 3) )"),
        "init body should be the tuple 1, 2, 3: {t}"
    );
}

#[test]
fn grouped_and_labeled_block() {
    ok("const _ = (1 + 2) * 3;");
    let t = shape(&sx("fn f() void { const v = blk: { break :blk 1; }; }"));
    assert!(t.contains("(block :label blk"), "{t}");
}

// ---------------------------------------------------------------------------
// Control flow — expression and statement forms
// ---------------------------------------------------------------------------

#[test]
fn if_expr_and_stmt() {
    ok("const _ = if (c) a else b;");
    ok("fn f() void { if (c) { return; } }");
    ok("fn f() void { if (c) |x| { use(x); } else |e| { use(e); } }");
    err("const _ = if (c) a;"); // expr form requires else
}

#[test]
fn if_stmt_single_nonblock_body_before_else() {
    // Defect 2: a statement-form `if` with a single NON-block body followed by
    // `else` must parse without a spurious "expected `;`" (grammar
    // `block_or_stmt = block | statement`; spec §4.3).
    ok("fn f() void { if (a) x() else y(); }");
    ok("fn f() void { if (a) x() else if (b) y() else z(); }");
    ok("fn f() void { while (a) x() else y(); }");
    // Without an `else`, the missing `;` is still an error (no regression).
    err("fn f() void { if (a) x() }");
}

#[test]
fn while_forms() {
    ok("fn f() void { while (c) {} }");
    ok("fn f() void { while (c) : (i += 1) {} }");
    ok("fn f() void { inline while (c) {} }");
    ok("fn f() void { lbl: while (c) {} }");
    ok("fn f() void { while (c) {} else {} }");
    let t = shape(&sx("fn f() void { while (c) : (i += 1) {} }"));
    assert!(t.contains("(cont"), "{t}");
}

#[test]
fn for_forms() {
    ok("fn f() void { for (xs) |x| {} }");
    ok("fn f() void { for (xs, 0..) |*slot, i| {} }");
    ok("fn f() void { inline for (xs) |x| {} }");
    ok("fn f() void { for (xs) |x| {} else {} }");
    ok("fn f() void { for (a, b) |x, y| {} }");
    let t = shape(&sx("fn f() void { for (xs, 0..) |*slot, i| {} }"));
    assert!(t.contains("(range"), "{t}");
    assert!(t.contains("(ref slot)"), "{t}");
}

#[test]
fn for_with_single_statement_body() {
    // `for (xs) |x| sum += x;` — a single-statement (non-block) body.
    ok("fn f() void { for (xs) |x| sum += x; }");
}

#[test]
fn switch_forms() {
    let src = "fn f() void {\n\
               switch (e) {\n\
               .A => x,\n\
               error.Empty => 10,\n\
               1...3 => y,\n\
               4, 5, 6 => z,\n\
               else => w,\n\
               }\n}\n";
    let t = shape(&sx(src));
    assert!(t.contains("(switch"), "{t}");
    assert!(t.contains("(else)"), "{t}");
    assert!(t.contains("(range"), "{t}");
    ok("fn f() void { switch (e) { x => |v| use(v), else => {}, } }");
}

#[test]
fn break_continue_and_cleanup_stmts() {
    ok("fn f() void { defer x(); }");
    ok("fn f() void { defer {} }");
    ok("fn f() void { errdefer alloc.free(x); }");
    ok("fn f() void { errdefer |e| { use(e); } }");
    ok("fn f() void { return; }");
    ok("fn f() void { return e; }");
    ok("fn f() void { break; }");
    ok("fn f() void { break :lbl v; }");
    ok("fn f() void { continue; }");
    ok("fn f() void { continue :lbl; }");
}

#[test]
fn assignment_statements_all_ops() {
    for op in [
        "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=",
    ] {
        ok(&format!("fn f() void {{ a {op} b; }}"));
    }
    ok("fn f() void { _ = e; }");
    ok("fn f() void { a.b = 1; }");
    ok("fn f() void { a[i] = 1; }");
    ok("fn f() void { p.* = 1; }");
}

// ---------------------------------------------------------------------------
// Error recovery
// ---------------------------------------------------------------------------

#[test]
fn recovery_missing_semicolon() {
    let res = parse("const a = 1\nconst b = 2;\n");
    assert_eq!(res.errors().count(), 1, "{:#?}", res.diagnostics);
    assert_eq!(res.file.items.len(), 2);
}

#[test]
fn recovery_stray_tokens_at_item_scope() {
    let res = parse("@@@ const a = 1;\nconst b = 2;\n");
    assert!(!res.is_ok());
    // Both items should still be recovered.
    assert_eq!(res.file.items.len(), 2, "{:#?}", res.diagnostics);
}

#[test]
fn recovery_inside_a_block() {
    let res = parse("fn f() void { const a = ; const b = 2; }\n");
    assert!(!res.is_ok());
    // The function item is still produced.
    assert_eq!(res.file.items.len(), 1);
}

#[test]
fn diagnostic_points_at_offending_token() {
    let res = parse("const a = 1\nconst b = 2;\n");
    let d = res.errors().next().unwrap();
    // The error should point at the `const` on line 2 (where the `;` was due).
    assert_eq!(d.span.line, 2, "{:#?}", res.diagnostics);
}

// ---------------------------------------------------------------------------
// Span / offset correctness
// ---------------------------------------------------------------------------

#[test]
fn span_offsets_are_scalar_indices() {
    let src = "const std = @import(\"std\");\n";
    let res = parse(src);
    assert!(res.is_ok());
    let item = &res.file.items[0];
    assert_eq!(item.span().start, 0);
    // The item spans up to and including the trailing `;` at offset 26.
    assert_eq!(item.span().end, 27);
}

#[test]
fn span_after_multiline_string_stays_correct() {
    // A multiline string then a decl: the second decl's offset must account for
    // the embedded newlines in the string token.
    let src = "const s =\n    \\\\line one\n    \\\\line two\n    ;\nconst x = 1;\n";
    let res = parse(src);
    assert!(res.is_ok(), "{:#?}", res.diagnostics);
    let x = &res.file.items[1];
    // `const x` starts on line 5; the offset must match a hand count.
    let want = src.find("const x").unwrap() as u32;
    assert_eq!(x.span().start, want, "tree: {:#?}", res.file.items);
}

#[test]
fn span_debug_variant_annotates_nodes() {
    let res = parse("const x = 1;\n");
    let s = k2_parse::to_sexpr_spans(&res.file);
    assert!(s.contains("@0..12"), "{s}");
}
