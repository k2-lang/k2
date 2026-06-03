//! Whole-file acceptance corpus: every `examples/*.k2` must type-check with zero
//! error-severity diagnostics.
//!
//! This is the hard acceptance gate of the milestone. The comptime-deferral
//! boundary (generic instantiation, reflection builtins, module/`anytype` member
//! access) is precisely calibrated so the examples check clean WITHOUT weakening
//! the concrete-core checks the negative suite verifies. If an example cannot
//! check cleanly, that signals a missing real type feature — not a reason to
//! widen `Deferred`.

use std::path::PathBuf;

use k2_types::{check_file, dump_signatures};

/// The absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn all_examples_check_clean() {
    let dir = examples_dir();
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("examples dir must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("k2") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let pres = k2_parse::parse(&src);
        assert!(
            pres.is_ok(),
            "{} did not parse cleanly: {:#?}",
            path.display(),
            pres.diagnostics
        );
        let resolved = k2_resolve::resolve_file(&pres.file);
        assert!(
            resolved.is_ok(),
            "{} did not resolve cleanly: {:#?}",
            path.display(),
            resolved.errors().collect::<Vec<_>>()
        );

        let typed = check_file(&pres.file, &resolved);
        assert!(
            typed.is_ok(),
            "{} did not type-check cleanly:\n{:#?}",
            path.display(),
            typed.errors().collect::<Vec<_>>()
        );

        // The signature dump must be non-empty and deterministic.
        let sigs = dump_signatures(&typed, &resolved);
        assert!(
            !sigs.is_empty(),
            "{}: expected a non-empty signature dump",
            path.display()
        );
        let sigs2 = dump_signatures(&typed, &resolved);
        assert_eq!(
            sigs,
            sigs2,
            "{}: dump must be deterministic",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected at least 6 example files, saw {checked}"
    );
}

// =========================================================================
//  v0.6 deferred-reduction: user generics & reflection are now CONCRETE,
//  while std/sys/build members stay Deferred (no std until v0.10).
// =========================================================================

use k2_syntax::Span;
use k2_types::{Type, TypeId, Typed};

/// Loads, parses, resolves, and type-checks one example file.
fn check_example(name: &str) -> (String, Typed) {
    let path = examples_dir().join(name);
    let src = std::fs::read_to_string(&path).unwrap();
    let pres = k2_parse::parse(&src);
    assert!(pres.is_ok(), "{name} parse: {:#?}", pres.diagnostics);
    let resolved = k2_resolve::resolve_file(&pres.file);
    assert!(resolved.is_ok(), "{name} resolve");
    let typed = k2_types::check_file(&pres.file, &resolved);
    (src, typed)
}

/// The recorded type at the first occurrence of `needle` that HAS a recorded
/// type (skipping comment-text occurrences, which record nothing).
///
/// Spans are in *scalar* (char) offsets — the example files use non-ASCII
/// em-dashes in comments — so a byte offset from `str::find` is converted to a
/// char offset before keying the per-occurrence type map.
fn first_type(t: &Typed, src: &str, needle: &str) -> Option<TypeId> {
    let mut start = 0usize;
    while let Some(rel) = src[start..].find(needle) {
        let byte = start + rel;
        start = byte + needle.len();
        let s = src[..byte].chars().count() as u32;
        let e = s + needle.chars().count() as u32;
        if let Some(ty) = t.type_at(Span::new(s, e, 1, 1)) {
            return Some(ty);
        }
    }
    None
}

#[test]
fn generic_list_instantiations_are_concrete() {
    let (src, t) = check_example("generic_list.k2");
    assert!(t.is_ok(), "generic_list must check clean");
    // The first CODE occurrence of `List(u32)` (the leading comment also names
    // it, but records no type) is a concrete struct type — no longer Deferred.
    let lu32 = first_type(&t, &src, "List(u32)").expect("List(u32) recorded");
    assert!(
        matches!(t.arena.get(lu32), Type::Struct(_)),
        "List(u32) should be a concrete Struct, got {:?}",
        t.arena.get(lu32)
    );
    // `List(u32)` and `List([]const u8)` are DISTINCT instantiations.
    let lstr = first_type(&t, &src, "List([]const u8)").expect("List([]const u8) recorded");
    assert!(matches!(t.arena.get(lstr), Type::Struct(_)));
    assert_ne!(lu32, lstr, "distinct element types -> distinct List types");
    // `nums.len` resolves to a concrete `usize`, not Deferred.
    let len = first_type(&t, &src, "nums.len").expect("nums.len recorded");
    assert!(
        matches!(
            t.arena.get(len),
            Type::Int {
                bits: k2_types::IntBits::Usize,
                ..
            }
        ),
        "nums.len should be usize, got {:?}",
        t.arena.get(len)
    );
    // `nums.slice()` returns a concrete `[]const u32` (the element is u32).
    let sl = first_type(&t, &src, "nums.slice()").expect("nums.slice() recorded");
    assert!(
        matches!(t.arena.get(sl), Type::Slice { is_const: true, .. }),
        "nums.slice() should be a const slice, got {:?}",
        t.arena.get(sl)
    );
}

#[test]
fn reflection_example_sizes_are_concrete() {
    let (src, t) = check_example("comptime_reflection.k2");
    assert!(t.is_ok(), "comptime_reflection must check clean");
    // `serializedSize(Packet)` is a concrete `usize` (the call result type).
    let sz = first_type(&t, &src, "serializedSize(Packet)").expect("serializedSize recorded");
    assert!(
        matches!(
            t.arena.get(sz),
            Type::Int {
                bits: k2_types::IntBits::Usize,
                ..
            }
        ),
        "serializedSize(Packet) should be usize, got {:?}",
        t.arena.get(sz)
    );
    // `&buf` is `*[7]u8` — the `[serializedSize(Packet)]u8` length resolved to 7.
    let buf_ptr = first_type(&t, &src, "&buf").expect("&buf recorded");
    match t.arena.get(buf_ptr) {
        Type::Pointer { pointee, .. } => match t.arena.get(*pointee) {
            Type::Array { len, .. } => assert_eq!(
                *len,
                k2_types::ArrayLen::Known(7),
                "the serializer buffer must be [7]u8"
            ),
            other => panic!("expected *[7]u8, got pointer to {other:?}"),
        },
        other => panic!("expected *[7]u8, got {other:?}"),
    }
}

#[test]
fn std_members_stay_deferred() {
    // The std/sys/build namespaces are opaque until v0.10: member access on them
    // must stay Deferred (the engine must NOT over-eagerly evaluate them).
    let (src, t) = check_example("hello.k2");
    assert!(t.is_ok());
    let stdout = first_type(&t, &src, "sys.io").expect("sys.io recorded");
    assert!(
        t.arena.is_bottom(stdout),
        "sys.io must stay Deferred/opaque, got {:?}",
        t.arena.get(stdout)
    );
}

#[test]
fn build_namespace_stays_deferred() {
    // The build script's `b.addLibrary(.{...})` etc. route through the opaque
    // `*Build` capability and must remain Deferred (std/build is v0.10).
    let (src, t) = check_example("build.k2");
    assert!(t.is_ok(), "build.k2 must check clean");
    let ty = first_type(&t, &src, "b.standardTarget").expect("b.standardTarget recorded");
    assert!(
        t.arena.is_bottom(ty),
        "b.standardTarget must stay Deferred, got {:?}",
        t.arena.get(ty)
    );
}
