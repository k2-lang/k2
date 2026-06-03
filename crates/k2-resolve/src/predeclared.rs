//! The fixed set of predeclared identifiers, visible in every k2 program.
//!
//! These names are not keywords (the lexer treats them as ordinary
//! identifiers); their meaning is assigned here, during name resolution, by
//! seeding the root scope with one [`DefKind::Predeclared`](crate::def::DefKind)
//! definition per name. Per spec §01 5.3 they are "predeclared identifiers,
//! visible everywhere unless shadowed", so a user binding may legally reuse any
//! of these names (and the resolver does *not* treat that as an illegal shadow).
//!
//! ## Why this is a deliberate superset of spec §01 5.3
//!
//! The spec's §01 5.3 list is the *lexical* minimum (`i8..usize`, `f32`/`f64`,
//! `bool void type noreturn anyerror comptime_int comptime_float`). The set
//! below adds three groups, each justified and chosen so the canonical examples
//! resolve with zero diagnostics:
//!
//! * **`f16` / `f128`** — referenced by the type/coercion rules in §02; rounding
//!   out the IEEE widths the language defines.
//! * **`anyopaque`** — §02 names it an "ordinary predeclared identifier (like
//!   `void`, `type`, …)".
//! * **The capability types `System`, `Allocator`, `Build`** — every example's
//!   entry point takes a bare `*System`; `allocators.k2`/`errors.k2`/
//!   `generic_list.k2` take a bare `Allocator`; `build.k2` takes a bare
//!   `*Build`. §04/§08 establish these as the predeclared capability handles, so
//!   they must resolve without an explicit import.
//! * **The C-interop integer aliases (`c_int`, …)** — a permissive superset for
//!   `extern fn` interop (§04). None appear in the current corpus, so they are
//!   harmless, but including them avoids a future false positive.
//!
//! * **`anytype`** — the type-erased generic marker. It is its own `Expr`
//!   variant only in *parameter* position (the parser emits `Expr::AnyType`
//!   from `parse_param_type`); in a struct-field type, a `var` type, or a
//!   return type the parser hands the resolver a bare `Expr::Ident{"anytype"}`
//!   instead (spec §07 992 spells a field `value: anytype`). Predeclaring the
//!   name makes that bare identifier resolve as a marker everywhere, removing
//!   the false positive without making the resolver depend on the parser
//!   emitting `Expr::AnyType` in every type position.
//!
//! Note what is *not* here: `true`/`false`/`null`/`undefined` are literal
//! keywords (their own `Expr` variants), `unreachable` is its own `Expr`
//! variant too, and `_` is the discard — none are identifier references, so
//! none need a predeclared `Def`. The arbitrary-width integer family
//! (`u0`..`u65535`, `i0`..`i65535`) is *not* enumerated here either: it is an
//! open pattern (`^[ui][0-9]+$` within the width bound), recognized directly in
//! the resolver's identifier lookup rather than listed name-by-name (see
//! `resolver::primitive_int_width`). `Self` is *not* global: in the corpus
//! every container declares `const Self = @This();` explicitly, so `Self`
//! resolves as an ordinary container item and no per-container injection is
//! needed (documented in the resolver).

/// Every predeclared identifier name, in a stable order. Seeding the root scope
/// from this slice gives the predeclared `Def`s deterministic ids.
pub const PREDECLARED: &[&str] = &[
    // ---- Signed integers ------------------------------------------------
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize", //
    // ---- Unsigned integers ----------------------------------------------
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize", //
    // ---- Floating point (incl. the f16/f128 widths from §02) ------------
    "f16",
    "f32",
    "f64",
    "f128", //
    // ---- Other primitive / abstract types -------------------------------
    "bool",
    "void",
    "type",
    "noreturn",
    "anyerror",
    "anyopaque",
    "comptime_int",
    "comptime_float",
    // The type-erased generic marker (also an `Expr::AnyType` in param
    // position; predeclared so the bare-identifier spelling resolves too).
    "anytype",
    // ---- C-interop integer aliases (§04 extern interop; permissive) -----
    "c_char",
    "c_short",
    "c_ushort",
    "c_int",
    "c_uint",
    "c_long",
    "c_ulong",
    "c_longlong",
    "c_ulonglong",
    "c_longdouble",
    // ---- Capability types (used bare in the examples) -------------------
    "System",
    "Allocator",
    "Build",
];
