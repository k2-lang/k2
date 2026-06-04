//! The native backend's byte-layout oracle.
//!
//! The VM is a *tagged-value* interpreter ([`k2_vm::value::Value`]) and never
//! observes byte layout or padding; the only place a numeric layout fact is
//! observable across the VM/native boundary is `@sizeOf`/`@alignOf`/`@offsetOf`,
//! whose values are produced by `k2_types::reflect::layout_depth` and folded into
//! the MIR as `Const::Int`. So for native code to put aggregates in memory in a
//! way that agrees with those folded constants — and with itself across function
//! boundaries — it must reproduce `reflect::layout_depth` **exactly**.
//!
//! This module is a faithful, self-contained port of that algorithm against the
//! same [`k2_types::TypeArena`] the MIR carries. The rules (verified line-by-line
//! against `reflect.rs`):
//!
//! * `Int{Fixed(n)}` → `size = align = ceil(n/8).next_power_of_two()`; `usize`/
//!   `isize` → 8. `Float{bits}` → `ceil(bits/8).next_power_of_two()`. `Bool` →
//!   1/1. `Void`/`NoReturn` → 0/1. `Pointer` → 8/8. `Slice` → **16/8** (ptr at
//!   +0, len at +8).
//! * `Optional(inner)`: `?*T` reuses the pointer layout (null niche, no flag);
//!   else `align = max(inner.align, 1)`, `size = round_up(inner.size + 1, align)`
//!   with the **flag byte after the payload** at `inner.size`.
//! * `ErrorUnion{ok}`: `align = max(ok.align, 2)`, a **`u16` tag at +0**, payload
//!   at `round_up(2, ok.align.max(1))`, `size = round_up(payload_off + ok.size,
//!   align)`.
//! * `Array{len,elem}`: `size = elem.size * len`, `align = elem.align`; elements
//!   contiguous at `i * elem.size`.
//! * `Struct`: fields in declaration order; `offset = round_up(offset, f.align);
//!   field_off = offset; offset += f.size`; `align = max field align`; `size =
//!   round_up(offset, align)`.
//! * `Enum(id)`: the layout of its tag integer.
//!
//! A non-layoutable type (an unresolved `Deferred`, a capability `Opaque`, an
//! inferred-length array) yields `None`; callers treat such a local as a scalar
//! handle (8 bytes) — the only `Deferred` locals in practice are the threaded
//! `*System`/writer-token capabilities the subset already handles as opaque
//! pointers.

use k2_types::{ArrayLen, IntBits, Type, TypeArena, TypeId};

/// The size and alignment of a type, in bytes. Mirrors `reflect::Layout`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// The total size in bytes (`@sizeOf`).
    pub size: u64,
    /// The alignment in bytes (`@alignOf`), always a power of two `>= 1`.
    pub align: u64,
}

impl Layout {
    /// A scalar handle layout (8/8) — the fallback for an opaque/capability/
    /// pointer-like local with no structural layout.
    pub const WORD: Layout = Layout { size: 8, align: 8 };
}

/// Rounds `x` up to the next multiple of `a` (a power of two). Mirrors
/// `reflect::round_up` exactly, including the `a <= 1` identity case.
pub fn round_up(x: u64, a: u64) -> u64 {
    if a <= 1 {
        return x;
    }
    (x + a - 1) & !(a - 1)
}

/// The byte size of an integer type — `ceil(n/8).next_power_of_two()` for a fixed
/// width, 8 for `usize`/`isize`, 0 for a zero-width int. Mirrors
/// `reflect::int_byte_size`.
pub fn int_byte_size(bits: IntBits) -> u64 {
    match bits {
        IntBits::Usize | IntBits::Isize => 8,
        IntBits::Fixed(0) => 0,
        IntBits::Fixed(n) => (n as u64).div_ceil(8).next_power_of_two(),
    }
}

/// The [`Layout`] of `ty`, or `None` for a non-layoutable type (an unresolved
/// `Deferred`/`AnyType`, an inferred-length array, a capability `Opaque`, etc.).
/// A faithful port of `reflect::layout_depth`, including the depth-64 cycle guard.
pub fn layout_of(arena: &TypeArena, ty: TypeId) -> Option<Layout> {
    layout_depth(arena, ty, 0)
}

/// The depth-guarded recursion behind [`layout_of`].
fn layout_depth(arena: &TypeArena, ty: TypeId, depth: u32) -> Option<Layout> {
    if depth > 64 {
        return None;
    }
    match arena.get(ty).clone() {
        Type::Int { bits, .. } => {
            let size = int_byte_size(bits);
            Some(Layout {
                size,
                align: size.max(1),
            })
        }
        Type::Float { bits } => {
            let size = (bits as u64).div_ceil(8).next_power_of_two();
            Some(Layout { size, align: size })
        }
        Type::Bool => Some(Layout { size: 1, align: 1 }),
        Type::Void | Type::NoReturn => Some(Layout { size: 0, align: 1 }),
        Type::Pointer { .. } => Some(Layout { size: 8, align: 8 }),
        // A capability / opaque handle (`Allocator`, a `*System`-derived token) is a
        // word-sized scalar in the native model — the handle id flows through a
        // register and is stored as an 8-byte value. Giving it a layout lets a
        // monomorphized container that *holds* an allocator (e.g. `ArrayList`'s
        // `alloc: Allocator` field) compute its struct layout, so the container is
        // classified MEMORY (sret) rather than mistakenly returned in one register.
        Type::Opaque(_) | Type::AnyOpaque => Some(Layout { size: 8, align: 8 }),
        Type::Slice { .. } => Some(Layout { size: 16, align: 8 }),
        Type::Optional(inner) => {
            let il = layout_depth(arena, inner, depth + 1)?;
            if matches!(arena.get(inner), Type::Pointer { .. }) {
                Some(il)
            } else {
                let align = il.align.max(1);
                let size = round_up(il.size + 1, align);
                Some(Layout { size, align })
            }
        }
        Type::ErrorUnion { ok, .. } => {
            let ol = layout_depth(arena, ok, depth + 1)?;
            let align = ol.align.max(2);
            let size = round_up(round_up(2, ol.align.max(1)) + ol.size, align);
            Some(Layout { size, align })
        }
        Type::Array { len, elem } => {
            let n = match len {
                ArrayLen::Known(n) => n,
                _ => return None,
            };
            let el = layout_depth(arena, elem, depth + 1)?;
            Some(Layout {
                size: el.size.saturating_mul(n),
                align: el.align.max(1),
            })
        }
        Type::Struct(id) => {
            let fields = arena.structs[id.0 as usize].fields.clone();
            let mut offset = 0u64;
            let mut max_align = 1u64;
            for f in &fields {
                let fl = layout_depth(arena, f.ty, depth + 1)?;
                offset = round_up(offset, fl.align.max(1));
                offset += fl.size;
                max_align = max_align.max(fl.align);
            }
            Some(Layout {
                size: round_up(offset, max_align),
                align: max_align,
            })
        }
        Type::Enum(id) => {
            let tag = arena.enums[id.0 as usize].tag;
            layout_depth(arena, tag, depth + 1)
        }
        _ => None,
    }
}

/// The byte offset of each field of a struct type, in declaration (layout) order,
/// using the same running-offset loop as `reflect::layout_depth`'s struct arm.
/// Returns an empty vec for a non-struct type.
pub fn field_offsets(arena: &TypeArena, struct_ty: TypeId) -> Vec<u64> {
    let Type::Struct(id) = arena.get(struct_ty) else {
        return Vec::new();
    };
    let fields = arena.structs[id.0 as usize].fields.clone();
    let mut offset = 0u64;
    let mut out = Vec::with_capacity(fields.len());
    for f in &fields {
        let fl = layout_of(arena, f.ty).unwrap_or(Layout::WORD);
        offset = round_up(offset, fl.align.max(1));
        out.push(offset);
        offset += fl.size;
    }
    out
}

/// The element byte size of an array or slice element type (the stride of an
/// `Index` projection). Falls back to a word for a non-layoutable element.
pub fn elem_size(arena: &TypeArena, container_ty: TypeId) -> u64 {
    let elem = match arena.get(container_ty) {
        Type::Array { elem, .. } | Type::Slice { elem, .. } => *elem,
        // A pointer-as-array (rare) or already an element type: size it directly.
        _ => return layout_of(arena, container_ty).map(|l| l.size).unwrap_or(8),
    };
    layout_of(arena, elem).map(|l| l.size).unwrap_or(1)
}

/// The byte offset of an error union's payload (`round_up(2, ok.align.max(1))`).
pub fn error_union_payload_off(arena: &TypeArena, eu_ty: TypeId) -> u64 {
    if let Type::ErrorUnion { ok, .. } = arena.get(eu_ty) {
        let oa = layout_of(arena, *ok).map(|l| l.align).unwrap_or(1).max(1);
        round_up(2, oa)
    } else {
        0
    }
}

/// The byte offset of an optional's flag byte (`inner.size`), or `None` for a
/// pointer-niche optional (`?*T`), which has no flag byte.
pub fn optional_flag_off(arena: &TypeArena, opt_ty: TypeId) -> Option<u64> {
    if let Type::Optional(inner) = arena.get(opt_ty) {
        if matches!(arena.get(*inner), Type::Pointer { .. }) {
            return None;
        }
        return layout_of(arena, *inner).map(|l| l.size);
    }
    None
}
