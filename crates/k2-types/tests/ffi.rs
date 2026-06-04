//! v0.19 C-interop typing tests: the `c_*` integer aliases are concrete C-ABI
//! widths, a many-item / sentinel pointer (`[*:0]const u8`) is a raw pointer, a
//! string literal coerces to a `const char *`, and the `extern`/`export`
//! FFI-representability + body-presence gates fire on the right shapes.

use k2_types::{check_file, ExternKind, IntBits, Type, Typed};

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

/// `true` if some error contains `frag`.
fn has_error_containing(t: &Typed, frag: &str) -> bool {
    t.errors().any(|d| d.message.contains(frag))
}

#[test]
fn c_int_is_a_concrete_i32() {
    // A `c_int` const lays out + types as a 32-bit signed int.
    let t = assert_clean("const x: c_int = 7;\n");
    // Find the type bound to the const binding via its declared type position.
    // We re-evaluate by checking a function using @sizeOf would fold; here we just
    // assert the predeclared mapping by checking a `c_int`-typed param signature.
    let src = "fn f(a: c_int) c_int { return a; }\n";
    let t2 = assert_clean(src);
    // The fn's signature param type should be Int{signed, Fixed(32)}.
    let _ = t; // first snippet just proves clean typing
    let mut found = false;
    for (_, ty) in t2.binding_types.iter() {
        if matches!(
            t2.arena.get(*ty),
            Type::Int {
                signed: true,
                bits: IntBits::Fixed(32)
            }
        ) {
            found = true;
        }
    }
    assert!(found, "c_int binds to i32 (signed Fixed(32))");
}

#[test]
fn c_long_and_c_char_widths() {
    let src = "fn f(a: c_long, b: c_char, c: c_uint) void { _ = a; _ = b; _ = c; }\n";
    let t = assert_clean(src);
    let mut have_long = false;
    let mut have_char = false;
    let mut have_uint = false;
    for (_, ty) in t.binding_types.iter() {
        match t.arena.get(*ty) {
            Type::Int {
                signed: true,
                bits: IntBits::Fixed(64),
            } => have_long = true,
            Type::Int {
                signed: true,
                bits: IntBits::Fixed(8),
            } => have_char = true,
            Type::Int {
                signed: false,
                bits: IntBits::Fixed(32),
            } => have_uint = true,
            _ => {}
        }
    }
    assert!(have_long, "c_long is i64");
    assert!(have_char, "c_char is i8 (signed on x86-64)");
    assert!(have_uint, "c_uint is u32");
}

#[test]
fn extern_fn_with_cstring_ptr_is_clean() {
    // The canonical `puts` declaration + a call passing a string literal.
    let src = r#"
        extern fn puts(s: [*:0]const u8) c_int;
        pub fn main() c_int { _ = puts("hi"); return 0; }
    "#;
    let t = assert_clean(src);
    // `puts` is recorded as an extern C function.
    assert!(
        t.extern_fns
            .values()
            .any(|info| info.kind == ExternKind::Extern && info.abi_name == "puts"),
        "puts recorded as extern C"
    );
}

#[test]
fn export_fn_is_recorded() {
    let t = assert_clean("export fn k2_add(a: c_int, b: c_int) c_int { return a + b; }\n");
    assert!(
        t.extern_fns
            .values()
            .any(|info| info.kind == ExternKind::Export && info.abi_name == "k2_add"),
        "k2_add recorded as export C"
    );
}

#[test]
fn variadic_extern_records_varargs() {
    let src = r#"
        extern fn printf(fmt: [*:0]const u8, ...) c_int;
        pub fn main() c_int { _ = printf("%d", 1); return 0; }
    "#;
    let t = assert_clean(src);
    assert!(
        t.extern_fns
            .values()
            .any(|info| info.abi_name == "printf" && info.varargs),
        "printf recorded as a varargs extern"
    );
}

#[test]
fn extern_fn_with_body_is_rejected() {
    let src = "extern fn bad() c_int { return 0; }\n";
    let t = check(src);
    assert!(
        has_error_containing(&t, "must not have a body"),
        "an extern fn with a body is an error"
    );
}

#[test]
fn export_fn_without_body_is_rejected() {
    let src = "export fn bad() c_int;\n";
    let t = check(src);
    assert!(
        has_error_containing(&t, "must have a body"),
        "an export fn without a body is an error"
    );
}

#[test]
fn extern_fn_with_slice_param_is_rejected() {
    // A `[]const u8` slice (a fat `{ptr,len}` aggregate) is NOT C-ABI representable.
    let src = "extern fn bad(s: []const u8) c_int;\n";
    let t = check(src);
    assert!(
        has_error_containing(&t, "not C-ABI representable"),
        "a slice param in an extern fn is an error"
    );
}

#[test]
fn extern_fn_with_optional_param_is_rejected() {
    let src = "extern fn bad(x: ?c_int) c_int;\n";
    let t = check(src);
    assert!(
        has_error_containing(&t, "not C-ABI representable"),
        "an optional param in an extern fn is an error"
    );
}

#[test]
fn many_ptr_without_sentinel_is_clean() {
    // `[*]const u8` (no sentinel) is also a raw pointer.
    let src = r#"
        extern fn use_ptr(p: [*]const u8) c_int;
        pub fn main() c_int { return 0; }
    "#;
    let t = assert_clean(src);
    assert!(
        t.extern_fns.values().any(|i| i.abi_name == "use_ptr"),
        "use_ptr recorded as extern"
    );
}
