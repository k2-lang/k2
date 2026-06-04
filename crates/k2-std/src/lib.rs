//! The bundled k2 standard library, embedded as k2 source.
//!
//! k2's `std` is written in k2 itself (see `std/std.k2`) and shipped inside the
//! compiler. [`STD_BODY`] is that source verbatim — the *body* of the `std`
//! namespace. The driver makes `@import("std")` resolve to a REAL compiled
//! module by prepending a single synthetic
//!
//! ```text
//! const std = struct { <STD_BODY> };
//! ```
//!
//! item into the user's source before resolve / check / lower, then re-pointing
//! the user's `const std = @import("std")` binding at it. Because `std` is then
//! an ordinary type-valued `const`, member access (`std.heap.GeneralPurposeAllocator`,
//! `std.ArrayList(u32)`) resolves to real declarations, monomorphizes through
//! the existing generic engine, and lowers to real MIR calls — not opaque
//! intrinsics. Only the handle-based allocator floor and the *System capability
//! readers bottom out in a small set of `@builtin` leaf intrinsics the VM
//! implements.
//!
//! The name chosen for the synthetic root ([`STD_ROOT_NAME`]) lives in the
//! compiler-reserved identifier space so it can never collide with a user name.

/// The verbatim k2 source of the standard library — the body of the `std`
/// namespace `struct`.
pub const STD_BODY: &str = include_str!("../std/std.k2");

/// The reserved identifier the synthetic `const <name> = struct { ... }` std
/// root binds to. It begins with `__k2_` so it is outside any name a user
/// program would write.
pub const STD_ROOT_NAME: &str = "__k2_std_root";

/// Builds the full k2 source of the synthetic std root item: a single
/// `const __k2_std_root = struct { <STD_BODY> };`. The driver concatenates this
/// ahead of the (import-rewritten) user source so the combined text is one
/// `SourceFile` the normal pipeline compiles.
pub fn std_root_item_source() -> String {
    format!("const {STD_ROOT_NAME} = struct {{\n{STD_BODY}\n}};\n")
}
