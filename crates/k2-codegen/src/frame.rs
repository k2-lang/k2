//! The per-function stack-frame planner.
//!
//! The emitted ELF has **no writable static segment** (`elf.rs` maps one R+X text
//! `PT_LOAD` and one R-only rodata `PT_LOAD`), so the only writable memory at
//! runtime is the stack. Every aggregate local, every spill slot, the hidden
//! `sret` destination pointer, and the print scratch buffer therefore live in the
//! current frame, laid out by this module once per function before lowering.
//!
//! The plan assigns, in descending `rbp`-relative offsets:
//!
//! 1. **Local homes** — a stack home for every local that needs one: any
//!    aggregate-typed local (struct/array/slice/optional/error-union, sized by
//!    [`crate::layout`]), any `address_taken` local (it needs a stable address for
//!    `Ref`), and any scalar the register allocator could not keep in a register
//!    (a *spill*). A scalar kept in a register has `home == None`.
//! 2. **Callee-saved save slots** — one 8-byte slot per callee-saved register the
//!    allocator actually assigned, written in the prologue and restored in the
//!    epilogue (we save into frame slots rather than `push`ing in the body, so RSP
//!    stays 16-aligned at every `call` with no per-call fixup).
//! 3. **The `sret` pointer slot** — when this function returns a MEMORY-class
//!    aggregate, the hidden destination pointer (passed in RDI) is stashed here.
//! 4. **The print scratch buffer** — a fixed region a `print` renders into before
//!    the `write` syscall (only reserved when the function prints).
//! 5. **The outgoing-args region** — space at the bottom of the frame for the
//!    stack arguments of any `>6`-int-arg / MEMORY-class call this function makes,
//!    so the body never `push`es and RSP alignment is preserved.
//!
//! `frame_size` is the 16-byte-aligned total; the prologue does
//! `push rbp; mov rbp,rsp; sub rsp, frame_size`, leaving RSP ≡ 0 (mod 16) at every
//! `call`.

use crate::layout::{self, Layout};
use crate::reg::Gpr;
use k2_mir::MirFunction;
use k2_types::{Type, TypeArena, TypeId};

/// A fixed, generous print scratch-buffer size. Every corpus `print` renders far
/// less than this; a single buffer is reused across all prints in the function.
pub const PRINT_BUF_SIZE: i32 = 8192;

/// The plan for one function's stack frame.
pub struct FramePlan {
    /// `local_home[i]` = the `rbp`-relative byte offset of local `i`'s memory home
    /// (negative), or `None` if the local lives only in a register.
    pub local_home: Vec<Option<i32>>,
    /// `rbp`-relative offset of the print scratch buffer base (negative), if the
    /// function prints.
    pub print_buf: Option<i32>,
    /// `rbp`-relative offset of the hidden `sret` pointer slot, if this function
    /// returns a MEMORY-class aggregate.
    pub ret_ptr_slot: Option<i32>,
    /// `rbp`-relative offsets of the callee-saved save slots, keyed by register.
    pub callee_saved_slots: Vec<(Gpr, i32)>,
    /// The 16-byte-aligned total frame size.
    pub frame_size: i32,
}

impl FramePlan {
    /// The `rsp`-relative byte offset of the start of the outgoing-args region.
    /// Stack call arguments are written at `[rsp + outgoing_args_base() + k]`.
    /// The region sits at the very bottom of the frame (lowest addresses).
    pub fn outgoing_args_base(&self) -> i32 {
        0
    }
}

/// `true` if a type is an aggregate that must live in memory (never a scalar
/// register): a struct, array, slice, tuple, non-pointer-niche optional, or error
/// union. Pointer-niche optionals (`?*T`) are scalar pointers.
pub fn is_memory_aggregate(arena: &TypeArena, ty: TypeId) -> bool {
    match arena.get(ty) {
        // A `@Vector(N, T)` is stored inline in the frame (like a small array),
        // not in a scalar register, so it needs a stack home for its literal /
        // per-lane SIMD lowering. (It is given a layout now, so without this arm
        // it would no longer be classified MEMORY and its aggregate literal could
        // not be lowered.)
        Type::Struct(_)
        | Type::Array { .. }
        | Type::Slice { .. }
        | Type::ErrorUnion { .. }
        | Type::Vector { .. } => true,
        Type::Optional(inner) => !matches!(arena.get(*inner), Type::Pointer { .. }),
        _ => false,
    }
}

/// Rounds a memory-aggregate home's *size* up to at least 8 bytes, leaving the
/// alignment untouched. The parameter-passing prologue stores each incoming
/// aggregate word with a full 8-byte `mov_store_mem` (and the caller reads it
/// back with an 8-byte `mov_load`), so a sub-8-byte aggregate (e.g. a packed
/// `struct { a: u3, b: u5 }` of size 1, or a plain `struct { a: u8, b: u8 }` of
/// size 2) whose home was sized to its exact byte count would have those 8-byte
/// stores spill 6–7 bytes into the *adjacent* frame slot, corrupting frame
/// bookkeeping and faulting a value-returning callee on return (SIGSEGV). The
/// aggregate's true byte size still drives `@sizeOf`/memcpy/field offsets; this
/// only pads the *reserved stack home* so the word-granular param store/load can
/// never overflow it. The pad is invisible to layout because it is local to the
/// frame planner — nothing reads `size` back out of the home.
fn pad_aggregate_home(l: Layout) -> Layout {
    Layout {
        size: l.size.max(8),
        align: l.align,
    }
}

/// The memory size/alignment to reserve for a local's home. Aggregates and wide
/// (>8-byte) integers use their natural layout; a narrow spilled scalar uses 8/8.
/// A sub-8-byte aggregate home is padded to 8 bytes (see [`pad_aggregate_home`]).
fn home_layout(arena: &TypeArena, ty: TypeId) -> Layout {
    if is_memory_aggregate(arena, ty) {
        return pad_aggregate_home(layout::layout_of(arena, ty).unwrap_or(Layout::WORD));
    }
    if let Some(l) = layout::layout_of(arena, ty) {
        if l.size > 8 {
            return l; // u128 / i128
        }
    }
    Layout::WORD
}

/// Plans the frame for `func`. `reg_home_needed[i]` is true if local `i` must
/// have a memory home despite being scalar (it was spilled by the allocator or is
/// `address_taken`); aggregates always get a home regardless. `saved` lists the
/// callee-saved registers the allocator assigned. `prints`/`returns_memory`/
/// `outgoing` size the auxiliary regions.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    func: &MirFunction,
    arena: &TypeArena,
    needs_home: &[bool],
    home_ty: &[Option<TypeId>],
    home_size: &[Option<(u64, u64)>],
    saved: &[Gpr],
    prints: bool,
    ret_ptr: bool,
    outgoing_args_size: i32,
) -> FramePlan {
    // Work in positive cumulative bytes growing downward from rbp, then negate.
    let mut cursor: u64 = 0;
    let mut local_home: Vec<Option<i32>> = vec![None; func.locals.len()];

    for (i, local) in func.locals.iter().enumerate() {
        let agg = is_memory_aggregate(arena, local.ty);
        if !agg && !needs_home[i] {
            continue; // lives in a register
        }
        // Size the home: an explicit (size, align) override (a `deferred` tuple)
        // wins; else the override type's layout; else the declared type's. Every
        // aggregate home is padded to at least 8 bytes so the word-granular
        // param store/load can never overflow it (see `pad_aggregate_home`).
        let l = if let Some((sz, al)) = home_size[i] {
            // A `home_size` override is only recorded for an aggregate home
            // (a deferred tuple/array literal), so pad it like any aggregate.
            pad_aggregate_home(Layout {
                size: sz,
                align: al,
            })
        } else {
            let size_ty = home_ty[i].unwrap_or(local.ty);
            if is_memory_aggregate(arena, size_ty) {
                pad_aggregate_home(layout::layout_of(arena, size_ty).unwrap_or(Layout::WORD))
            } else {
                home_layout(arena, local.ty)
            }
        };
        let size = l.size.max(1);
        let align = l.align.max(1);
        cursor = layout::round_up(cursor + size, align);
        // The home is the *lowest* address of the object: rbp - cursor.
        local_home[i] = Some(-(cursor as i32));
    }

    // Callee-saved save slots (8 bytes each).
    let mut callee_saved_slots = Vec::with_capacity(saved.len());
    for &r in saved {
        cursor = layout::round_up(cursor + 8, 8);
        callee_saved_slots.push((r, -(cursor as i32)));
    }

    // The hidden sret pointer slot (8 bytes).
    let ret_ptr_slot = if ret_ptr {
        cursor = layout::round_up(cursor + 8, 8);
        Some(-(cursor as i32))
    } else {
        None
    };

    // The print scratch buffer.
    let print_buf = if prints {
        cursor = layout::round_up(cursor + PRINT_BUF_SIZE as u64, 16);
        Some(-(cursor as i32))
    } else {
        None
    };

    // The outgoing-args region sits at the very bottom (lowest addresses), so it
    // is addressed from rsp at small positive offsets. Reserve it last.
    let outgoing = outgoing_args_size.max(0) as u64;
    cursor += outgoing;

    let frame_size = (layout::round_up(cursor, 16)) as i32;

    FramePlan {
        local_home,
        print_buf,
        ret_ptr_slot,
        callee_saved_slots,
        frame_size,
    }
}
