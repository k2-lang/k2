//! The runtime value model the VM operates on.
//!
//! Native memory layout is **not** modeled at this milestone (that is post-0.13
//! native codegen). Instead the VM works on a tagged [`Value`]: integers carry
//! their working width and signedness so every arithmetic result can be masked
//! and sign-extended without an arena lookup; aggregates are reference-counted
//! `Vec`s for cheap copy-on-read semantics; and pointers/slices are handles into
//! the [managed heap](crate::heap). The model is deliberately small — just rich
//! enough that the inserted safety checks, optional/error-union discriminants,
//! and the io/heap capabilities work — and every value is `Clone`, matching the
//! MIR's copy-on-read discipline (a place read is always a copy; mutation only
//! ever happens through an `Assign`).

use std::rc::Rc;

use k2_types::TypeId;

use crate::heap::Ptr;

/// The compact representation of an integer's width and signedness, resolved
/// once from the arena and cached so width-correct arithmetic, formatting, and
/// bounds math need no further type lookups.
///
/// A `comptime_int` (and any operand whose width could not be resolved) uses
/// `width == 0`, meaning "no mask" — the value is kept in full `i128` precision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntRepr {
    /// The bit width (`8`, `16`, … `128`; `64` for `usize`/`isize`). `0` means
    /// an unbounded `comptime_int` that is never masked.
    pub width: u16,
    /// `true` for a signed integer.
    pub signed: bool,
}

impl IntRepr {
    /// The unbounded `comptime_int` representation (no masking).
    pub const COMPTIME: IntRepr = IntRepr {
        width: 0,
        signed: true,
    };

    /// The `usize` representation (unsigned, 64-bit), used for slice lengths and
    /// indices.
    pub const USIZE: IntRepr = IntRepr {
        width: 64,
        signed: false,
    };

    /// Normalizes `v` to this representation: masks to the width and sign-extends
    /// the top bit for a signed type. A zero-width (`comptime_int`) repr returns
    /// `v` unchanged.
    pub fn normalize(self, v: i128) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return v;
        }
        let bits = self.width as u32;
        let mask: u128 = (1u128 << bits) - 1;
        let masked = (v as u128) & mask;
        if self.signed && (masked >> (bits - 1)) & 1 == 1 {
            // Sign-extend: set every bit above the width.
            (masked | !mask) as i128
        } else {
            masked as i128
        }
    }

    /// The inclusive maximum value representable in this repr (as `i128`).
    pub fn max_value(self) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return i128::MAX;
        }
        let bits = self.width as u32;
        if self.signed {
            (1i128 << (bits - 1)) - 1
        } else {
            (1i128 << bits) - 1
        }
    }

    /// The inclusive minimum value representable in this repr (as `i128`).
    pub fn min_value(self) -> i128 {
        if self.width == 0 || self.width >= 128 {
            return i128::MIN;
        }
        if self.signed {
            -(1i128 << (self.width as u32 - 1))
        } else {
            0
        }
    }
}

/// A runtime value. Every variant is `Clone`; aggregates share their backing via
/// [`Rc`] so a place read (always a copy in the MIR) is cheap, and mutation goes
/// through [`Rc::make_mut`] for copy-on-write correctness.
#[derive(Clone, Debug)]
pub enum Value {
    /// An integer, kept in full `i128` precision but normalized to `repr` on
    /// every store and arithmetic result.
    Int {
        /// The (already normalized) value.
        v: i128,
        /// The width/signedness this integer is masked to.
        repr: IntRepr,
    },
    /// An IEEE-754 double.
    Float(f64),
    /// A boolean.
    Bool(bool),
    /// `void` / the unit value.
    Unit,
    /// An owned byte run: a `[]const u8` string literal or an `@errorName`
    /// result. Shared via `Rc` for cheap copies.
    Str(Rc<Vec<u8>>),
    /// A handle into the managed heap (a single-item pointer, or an interior
    /// pointer into an allocation).
    Ptr(Ptr),
    /// A fat slice pointer: a data handle plus a length.
    Slice {
        /// The data pointer (`Ptr::NULL` for the empty slice).
        ptr: Ptr,
        /// The element count.
        len: usize,
    },
    /// A by-value array of `N` elements.
    Array(Rc<Vec<Value>>),
    /// A by-value struct/tuple: its fields in layout order.
    Struct(Rc<Vec<Value>>),
    /// A tagged-union value: the active variant index plus its payload.
    Enum {
        /// The active variant index.
        tag: u32,
        /// The variant payload (`Unit` for a payload-less variant).
        payload: Box<Value>,
    },
    /// An optional: `None` is `null`, `Some(v)` is present.
    Optional(Option<Box<Value>>),
    /// The success arm of an error union, `Ok(payload)`.
    ErrOk(Box<Value>),
    /// The error arm of an error union: its global error tag.
    ErrVal(u16),
    /// A runtime capability/opaque handle (the `*System` root and its `io`/`heap`
    /// children).
    Cap(Capability),
    /// A live scheduler-object handle: a Task/Future, Channel, Mutex, Atomic, or
    /// WaitGroup. The [`SchedKind`] disambiguates which scheduler table `id`
    /// indexes. Like [`Capability::Allocator`], this is a pure kind+id pair (no Rust
    /// state) — copying it shares the same underlying object, which is exactly the
    /// intended channel/mutex/task aliasing semantics.
    Sched {
        /// Which scheduler table `id` indexes.
        kind: SchedKind,
        /// The handle id into that table.
        id: u32,
    },
    /// A reference to a compiled function, carrying its [`k2_mir::FnId`]. This is
    /// the spawn tag: `Executor.spawn(work, …)` lowers `work` to an `FnRef` so the
    /// scheduler can build a fresh fiber whose root frame runs that function. (The
    /// MIR has no indirect calls, so a function passed by value is materialized as
    /// this const tag at the call site rather than a real first-class pointer.)
    FnRef(k2_mir::FnId),
    /// Undefined bits of a known type. Also carries a type for the `create(undef)`
    /// idiom, where the `undef` operand is a *type carrier* for the element type.
    Undef(TypeId),
}

/// The kind of scheduler object a [`Value::Sched`] handle indexes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedKind {
    /// A spawned task / awaitable future: `id` is a [`crate::sched::FiberId`].
    Task,
    /// A channel: `id` is a [`crate::sched::ChanId`].
    Channel,
    /// A mutex: `id` is a [`crate::sched::MutexId`].
    Mutex,
    /// An atomic cell: `id` is a [`crate::sched::AtomicId`].
    Atomic,
    /// A wait-group: `id` is a [`crate::sched::WgId`].
    WaitGroup,
}

/// A runtime capability handle. The shim hands `main` the [`Capability::System`]
/// root; the io/heap intrinsics derive the rest from it. None of these carry
/// real OS state — stdout/stderr are buffers owned by the VM, and the heap is the
/// managed `Heap`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capability {
    /// The root `*System` authority handed to `main`.
    System,
    /// The `sys.io` namespace capability.
    Io,
    /// A `Writer` over standard output.
    StdoutWriter,
    /// A `Writer` over standard error.
    StderrWriter,
    /// An `Allocator` capability, carrying its **handle id**: the index of an
    /// allocator instance in the VM's per-run registry. Id `0` is the
    /// program-wide default (`sys.heap`); the `std.heap.*` allocators mint
    /// further ids (GPA, arena, fixed-buffer) so different kinds dispatch
    /// differently — all without fn-pointer vtables.
    Allocator(u32),
    /// The `sys.clock` monotonic/wall reader capability.
    Clock,
    /// The `sys.random` PRNG capability.
    Random,
    /// The `sys.env` environment-lookup capability.
    Env,
}

impl Value {
    /// Builds a normalized integer value of `repr`.
    pub fn int(v: i128, repr: IntRepr) -> Value {
        Value::Int {
            v: repr.normalize(v),
            repr,
        }
    }

    /// Reads this value as an `i128`. Integers, booleans, enum tags, and error
    /// tags all read as their numeric value, so a `switch` on an enum/error-union
    /// scrutinee works without special-casing the discriminant at the call site.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Int { v, .. } => Some(*v),
            Value::Bool(b) => Some(*b as i128),
            Value::Enum { tag, .. } => Some(*tag as i128),
            Value::ErrVal(tag) => Some(*tag as i128),
            _ => None,
        }
    }

    /// Reads this value as a `usize` index/length, if it is a non-negative
    /// integer.
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Value::Int { v, .. } if *v >= 0 => Some(*v as usize),
            _ => None,
        }
    }

    /// Reads this value as a `bool`, if it is one (an integer `0`/`1` also reads
    /// as a bool, since comparison results and check predicates are booleans).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int { v, .. } => Some(*v != 0),
            _ => None,
        }
    }

    /// Reads this value as an `f64`, if it is a float (or an integer, which
    /// promotes).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int { v, .. } => Some(*v as f64),
            _ => None,
        }
    }
}
