//! Unit tests for the value model, the heap, and the formatter — the
//! component-level invariants the VM relies on. End-to-end execution of real k2
//! programs is covered by the integration tests in `tests/run.rs`.

use crate::fmt::format_into;
use crate::heap::{Heap, Ptr};
use crate::value::{IntRepr, Value};

/// A `u8` repr (unsigned, 8-bit).
fn u8r() -> IntRepr {
    IntRepr {
        width: 8,
        signed: false,
    }
}

/// An `i32` repr (signed, 32-bit).
fn i32r() -> IntRepr {
    IntRepr {
        width: 32,
        signed: true,
    }
}

#[test]
fn int_normalization_masks_and_sign_extends() {
    // u8 wraps 256 -> 0 and 300 -> 44.
    assert_eq!(u8r().normalize(256), 0);
    assert_eq!(u8r().normalize(300), 44);
    // i32 sign-extends a value with the top bit set.
    assert_eq!(i32r().normalize(0xFFFF_FFFF), -1);
    // comptime (width 0) never masks.
    assert_eq!(IntRepr::COMPTIME.normalize(1 << 100), 1 << 100);
}

#[test]
fn int_bounds_are_correct() {
    assert_eq!(u8r().max_value(), 255);
    assert_eq!(u8r().min_value(), 0);
    assert_eq!(i32r().max_value(), i32::MAX as i128);
    assert_eq!(i32r().min_value(), i32::MIN as i128);
}

#[test]
fn value_int_is_normalized_on_construction() {
    let v = Value::int(300, u8r());
    assert_eq!(v.as_i128(), Some(44));
}

#[test]
fn heap_create_load_store_roundtrips() {
    let mut h = Heap::new(false);
    let p = h.alloc_one(Value::int(7, i32r()));
    assert_eq!(h.load(p).unwrap().as_i128(), Some(7));
    h.store(p, Value::int(9, i32r())).unwrap();
    assert_eq!(h.load(p).unwrap().as_i128(), Some(9));
}

#[test]
fn heap_use_after_free_is_a_fault() {
    let mut h = Heap::new(false);
    let p = h.alloc_one(Value::Unit);
    h.free(p);
    assert!(h.load(p).is_err());
}

#[test]
fn heap_many_indexes() {
    let mut h = Heap::new(false);
    let p = h.alloc_many(Value::int(0, i32r()), 4).unwrap();
    h.store_index(p, 2, Value::int(42, i32r())).unwrap();
    assert_eq!(h.load_index(p, 2).unwrap().as_i128(), Some(42));
    assert_eq!(h.len_of(p), Some(4));
}

#[test]
fn null_pointer_load_is_a_fault() {
    let h = Heap::new(false);
    assert!(h.load(Ptr::NULL).is_err());
}

#[test]
fn one_array_cell_honors_offset_for_indexed_access() {
    // FINDING #3: a `CellData::One(Value::Array)` cell (a boxed `&storage` array,
    // the FBA backing shape) must index *into the inner array* via `ptr.offset` so
    // two sub-views into the same cell do not alias. `load`/`store` (whole-cell)
    // still see the whole array.
    let mut h = Heap::new(false);
    let arr = Value::Array(std::rc::Rc::new(vec![Value::int(0, u8r()); 6]));
    let base = h.alloc_one(arr);
    // Sub-view `a` = base[0..3], sub-view `b` = base[3..6].
    let a = base;
    let b = Ptr {
        cell: base.cell,
        offset: 3,
    };
    h.store_index(a, 0, Value::int(11, u8r())).unwrap();
    h.store_index(b, 0, Value::int(22, u8r())).unwrap();
    // Disjoint windows: a[0] is 11, b[0] is 22 (not aliased).
    assert_eq!(h.load_index(a, 0).unwrap().as_i128(), Some(11));
    assert_eq!(h.load_index(b, 0).unwrap().as_i128(), Some(22));
    // Whole-cell `load` still returns the whole array (length 6).
    assert!(matches!(h.load(base).unwrap(), Value::Array(a) if a.len() == 6));
    // Out-of-range interior access is a clean fault, not a panic.
    assert!(h.load_index(base, 6).is_err());
}

#[test]
fn oversized_alloc_is_a_clean_out_of_memory_fault() {
    // A request beyond the element cap must be rejected *before* any backing
    // `Vec` is touched, so the Rust global allocator never aborts the process.
    let mut h = Heap::new(false);
    let huge = crate::heap::MAX_ALLOC_ELEMS + 1;
    assert!(matches!(
        h.alloc_many(Value::Unit, huge),
        Err(crate::heap::HeapFault::OutOfMemory)
    ));
    // A 100-trillion-element request (the reviewer's repro) is likewise clean.
    assert!(matches!(
        h.alloc_many(Value::Unit, 100_000_000_000_000),
        Err(crate::heap::HeapFault::OutOfMemory)
    ));
    // A reasonable allocation still succeeds.
    assert!(h.alloc_many(Value::Unit, 8).is_ok());
}

#[test]
fn format_basic_placeholders() {
    let mut out = Vec::new();
    let args = vec![
        Value::Str(std::rc::Rc::new(b"Sol".to_vec())),
        Value::int(42, i32r()),
    ];
    format_into(&mut out, b"{s} = {d}\n", &args).unwrap();
    assert_eq!(out, b"Sol = 42\n");
}

#[test]
fn format_large_unsigned_magnitude() {
    // The hello.k2 acceptance value renders with no sign and full magnitude.
    let mut out = Vec::new();
    let big = 384_600_000_000_000_000_000_000_000i128;
    let args = vec![Value::int(
        big,
        IntRepr {
            width: 128,
            signed: false,
        },
    )];
    format_into(&mut out, b"~{d} W", &args).unwrap();
    assert_eq!(out, b"~384600000000000000000000000 W");
}

#[test]
fn format_escaped_braces_and_alignment() {
    let mut out = Vec::new();
    let args = vec![Value::Str(std::rc::Rc::new(b"12x".to_vec()))];
    // `{s:>14}` right-aligns within width 14.
    format_into(&mut out, b"{{{s:>14}}}", &args).unwrap();
    assert_eq!(out, b"{           12x}");
}

#[test]
fn format_hex_and_char() {
    let mut out = Vec::new();
    format_into(&mut out, b"{x}", &[Value::int(255, u8r())]).unwrap();
    assert_eq!(out, b"ff");
    let mut out = Vec::new();
    format_into(&mut out, b"{c}", &[Value::int(65, u8r())]).unwrap();
    assert_eq!(out, b"A");
}

#[test]
fn format_decimal_verb_on_float() {
    // `{d}` on a float renders the float, not the `<int>` placeholder.
    let mut out = Vec::new();
    format_into(&mut out, b"{d}", &[Value::Float(3.5)]).unwrap();
    assert_eq!(out, b"3.5");
}

#[test]
fn format_radix_masks_negative_to_width() {
    // A negative signed value prints its two's-complement at its *declared*
    // width, not the full 128-bit pattern.
    let mut out = Vec::new();
    format_into(&mut out, b"{x}", &[Value::int(-1, i32r())]).unwrap();
    assert_eq!(out, b"ffffffff");
    let mut out = Vec::new();
    format_into(
        &mut out,
        b"{x}",
        &[Value::int(
            -1,
            IntRepr {
                width: 8,
                signed: true,
            },
        )],
    )
    .unwrap();
    assert_eq!(out, b"ff");
    let mut out = Vec::new();
    format_into(
        &mut out,
        b"{b}",
        &[Value::int(
            -1,
            IntRepr {
                width: 8,
                signed: true,
            },
        )],
    )
    .unwrap();
    assert_eq!(out, b"11111111");
}
