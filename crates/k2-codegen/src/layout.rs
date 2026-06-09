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

use k2_types::{ArrayLen, IntBits, Type, TypeArena, TypeId, UnionTagKind};

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
        // `@Vector(N, T)` is laid out like a contiguous array of its element type,
        // but with an XMM-style alignment: `align = min(16, (N*elem).next_pow2())`
        // and `size = round_up(N*elem, align)`. This mirrors `reflect::layout_depth`
        // exactly so `@sizeOf`/`@alignOf` and the native byte image agree. Without
        // this arm `layout_of(@Vector)` was `None`, which routed a vector literal
        // through the synthetic-layout path and mis-strode bool-const lanes to
        // 8-byte stores (only lane 0 survived) — see `build_aggregate`.
        Type::Vector { len, elem } => {
            let el = layout_depth(arena, elem, depth + 1)?;
            let raw = el.size.saturating_mul(len as u64);
            let align = raw.max(1).next_power_of_two().min(16);
            Some(Layout {
                size: round_up(raw, align),
                align,
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
        // A `union(enum)` — a tag (discriminant) at +0 followed by a payload area
        // sized to the largest variant and aligned to the strictest. A faithful port
        // of the `Type::Union` arm in `reflect::layout_depth`; the two MUST agree, or
        // `@sizeOf`/`@offsetOf` (folded from `reflect`) would disagree with the bytes
        // native code reads/writes.
        Type::Union(id) => {
            let info = &arena.unions[id.0 as usize];
            let tag = match info.tag {
                UnionTagKind::None => Layout { size: 0, align: 1 },
                _ => {
                    let sz = int_byte_size(IntBits::Fixed(enum_tag_bits(info.variants.len())));
                    Layout {
                        size: sz,
                        align: sz.max(1),
                    }
                }
            };
            let variants = info.variants.clone();
            let mut payload_size = 0u64;
            let mut payload_align = 1u64;
            for v in &variants {
                let pl = layout_depth(arena, v.payload, depth + 1)?;
                payload_size = payload_size.max(pl.size);
                payload_align = payload_align.max(pl.align);
            }
            let align = tag.align.max(payload_align);
            let payload_off = round_up(tag.size, payload_align);
            Some(Layout {
                size: round_up(payload_off + payload_size, align),
                align,
            })
        }
        _ => None,
    }
}

/// The bit width of an inferred enum/union tag distinguishing `n` variants:
/// `max(1, ceil(log2(n)))`. A faithful copy of `k2_types::check::enum_tag_bits`
/// (a private helper in that crate); the two MUST agree so a union's tag is sized
/// identically here and in `reflect`.
fn enum_tag_bits(n: usize) -> u16 {
    if n <= 2 {
        return 1;
    }
    let bits = (usize::BITS - (n - 1).leading_zeros()) as u16;
    bits.max(1)
}

/// The byte offset of a tagged union's payload area: `round_up(tag_size,
/// max_variant_align)`. The tag sits at +0; every variant's payload begins here.
/// Returns 0 for a non-union type.
pub fn union_payload_off(arena: &TypeArena, union_ty: TypeId) -> u64 {
    let Type::Union(id) = arena.get(union_ty) else {
        return 0;
    };
    let info = &arena.unions[id.0 as usize];
    let tag_size = match info.tag {
        UnionTagKind::None => 0,
        _ => int_byte_size(IntBits::Fixed(enum_tag_bits(info.variants.len()))),
    };
    let mut payload_align = 1u64;
    for v in &info.variants {
        if let Some(pl) = layout_of(arena, v.payload) {
            payload_align = payload_align.max(pl.align);
        }
    }
    round_up(tag_size, payload_align)
}

/// The byte size and alignment of a tagged union's tag (discriminant) at +0.
/// `(0, 1)` for an untagged `union {…}`.
pub fn union_tag_layout(arena: &TypeArena, union_ty: TypeId) -> Layout {
    let Type::Union(id) = arena.get(union_ty) else {
        return Layout { size: 0, align: 1 };
    };
    let info = &arena.unions[id.0 as usize];
    match info.tag {
        UnionTagKind::None => Layout { size: 0, align: 1 },
        _ => {
            let sz = int_byte_size(IntBits::Fixed(enum_tag_bits(info.variants.len())));
            Layout {
                size: sz,
                align: sz.max(1),
            }
        }
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
        // A `@Vector(N, T)` strides by its element size just like an array, so a
        // vector literal's per-lane stores land at the correct 1/2/4/8-byte stride
        // (a bool vector stores 1-byte lanes, not 8-byte words).
        Type::Array { elem, .. } | Type::Slice { elem, .. } | Type::Vector { elem, .. } => *elem,
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

#[cfg(test)]
mod tests {
    use super::*;
    use k2_types::{UnionInfo, UnionTagKind, UnionVariant};

    fn span() -> k2_syntax::Span {
        k2_syntax::Span::new(0, 0, 0, 0)
    }

    /// Builds an inferred-tag `union(enum)` whose variants carry `payloads` (in
    /// order). The union's defining span is `start`, so two unions in one arena
    /// get distinct nominal identities (interning dedups by span).
    fn union_of(arena: &mut TypeArena, start: u32, payloads: &[TypeId]) -> TypeId {
        let variants = payloads
            .iter()
            .enumerate()
            .map(|(i, &p)| UnionVariant {
                name: format!("v{i}"),
                payload: p,
                span: span(),
            })
            .collect();
        arena.intern_union(UnionInfo {
            def: None,
            name: "U".to_string(),
            span: k2_syntax::Span::new(start, start + 1, 0, 0),
            tag: UnionTagKind::Inferred,
            variants,
            decls: Vec::new(),
        })
    }

    fn u(arena: &mut TypeArena, bits: u16) -> TypeId {
        arena.intern(Type::Int {
            signed: false,
            bits: IntBits::Fixed(bits),
        })
    }

    #[test]
    fn union_layout_is_tag_plus_max_payload() {
        let mut arena = TypeArena::new();
        let (u8t, u32t) = (u(&mut arena, 8), u(&mut arena, 32));
        // union(enum) { v0: u8, v1: u32 }: 2 variants -> 1-byte tag; payload max =
        // u32 (4, align 4); payload at round_up(1, 4) = 4; size round_up(4+4, 4)=8.
        let un = union_of(&mut arena, 100, &[u8t, u32t]);
        assert_eq!(layout_of(&arena, un), Some(Layout { size: 8, align: 4 }));
        assert_eq!(union_payload_off(&arena, un), 4);
        assert_eq!(union_tag_layout(&arena, un).size, 1);
    }

    #[test]
    fn union_layout_aligns_payload_to_widest_variant() {
        let mut arena = TypeArena::new();
        let boolt = arena.intern(Type::Bool);
        let u64t = u(&mut arena, 64);
        let voidt = arena.t_void();
        // union(enum) { v0: bool, v1: u64, v2: void }: 3 variants -> 1-byte tag;
        // payload max = u64 (8, align 8); payload at round_up(1, 8) = 8; size 16.
        let un = union_of(&mut arena, 200, &[boolt, u64t, voidt]);
        assert_eq!(layout_of(&arena, un), Some(Layout { size: 16, align: 8 }));
        assert_eq!(union_payload_off(&arena, un), 8);
        assert_eq!(union_tag_layout(&arena, un).size, 1);
    }

    #[test]
    fn union_payload_off_is_zero_for_a_non_union() {
        let mut arena = TypeArena::new();
        let u32t = u(&mut arena, 32);
        assert_eq!(union_payload_off(&arena, u32t), 0);
    }
}
