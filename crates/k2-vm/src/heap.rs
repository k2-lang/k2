//! The managed heap: a generationless arena of typed cells.
//!
//! The VM does not model native memory layout (that is post-0.13). A heap
//! allocation is a *cell* holding one [`Value`] (`create(T)`) or a run of values
//! (`alloc(T, n)`), and a [`Ptr`] names a cell plus an element offset. Freeing
//! marks the cell dead; a load/store through a dead cell becomes a clean
//! use-after-free panic instead of undefined behaviour (the liveness flag is
//! ignored in `ReleaseFast`, matching the spec's "checks stripped" semantics).
//!
//! Taking the address of a stack local (`&local`) also lives here: the local's
//! current value is boxed into a fresh cell and the register holds a [`Ptr`] to
//! it, so `&p`, `cell.* = ...`, and `&self` method receivers are all uniform
//! pointer operations.

use crate::value::Value;

/// A handle into the [`Heap`]: a cell index plus an element offset (the offset
/// indexes into a multi-element allocation; it is `0` for a single cell).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ptr {
    /// The cell index.
    pub cell: u32,
    /// The element offset within a multi-element cell.
    pub offset: usize,
}

impl Ptr {
    /// The null pointer sentinel (the data half of an empty slice / `null`).
    pub const NULL: Ptr = Ptr {
        cell: u32::MAX,
        offset: 0,
    };

    /// `true` if this is the null sentinel.
    pub fn is_null(self) -> bool {
        self.cell == u32::MAX
    }
}

/// The data behind a heap cell.
enum CellData {
    /// A single value (`create(T)`, or a boxed stack local).
    One(Value),
    /// A run of values (`alloc(T, n)`, also backing a heap-owned slice).
    Many(Vec<Value>),
}

/// One heap cell, with a liveness flag for use-after-free detection.
struct Cell {
    /// `false` once the cell has been freed.
    live: bool,
    /// The cell's contents.
    data: CellData,
}

/// A heap-access fault, surfaced to the VM as a clean panic rather than a Rust
/// panic.
#[derive(Clone, Debug)]
pub enum HeapFault {
    /// A load/store through a freed cell.
    UseAfterFree,
    /// A load/store through a null or otherwise invalid pointer.
    NullPointer,
    /// An interior offset past the end of an allocation.
    OutOfRange,
    /// A requested allocation was too large to satisfy (its element count
    /// exceeds the cap, or the host allocator could not reserve the backing
    /// run). Surfaced as a clean out-of-memory panic instead of letting the Rust
    /// global allocator's `handle_alloc_error` abort the whole process.
    OutOfMemory,
}

/// The largest number of elements a single `alloc(T, n)` may request. A request
/// beyond this is rejected with [`HeapFault::OutOfMemory`] *before* any backing
/// `Vec` is touched, so an astronomically large `n` (which would otherwise hand
/// the Rust global allocator a multi-terabyte request and trigger an
/// uncatchable `handle_alloc_error` abort) becomes a clean program panic. The
/// cap is far above anything the corpus allocates (a few thousand elements)
/// while keeping the worst-case backing `Vec` to a sane size.
pub const MAX_ALLOC_ELEMS: usize = 1 << 30;

/// The managed heap.
#[derive(Default)]
pub struct Heap {
    cells: Vec<Cell>,
    /// When `true` (ReleaseFast), the liveness flag is ignored on access.
    ignore_liveness: bool,
}

impl Heap {
    /// Builds an empty heap. `ignore_liveness` mirrors `ReleaseFast`, where
    /// use-after-free is undefined behaviour rather than a checked panic.
    pub fn new(ignore_liveness: bool) -> Heap {
        Heap {
            cells: Vec::new(),
            ignore_liveness,
        }
    }

    /// Allocates a single-value cell, returning a pointer to it.
    pub fn alloc_one(&mut self, value: Value) -> Ptr {
        let cell = self.cells.len() as u32;
        self.cells.push(Cell {
            live: true,
            data: CellData::One(value),
        });
        Ptr { cell, offset: 0 }
    }

    /// Allocates a multi-value cell of `n` copies of `init`, returning a pointer
    /// to its first element.
    ///
    /// The element count is validated against [`MAX_ALLOC_ELEMS`] and the backing
    /// run is grown with [`Vec::try_reserve_exact`] so an over-large request can
    /// **never** reach the global allocator's aborting `handle_alloc_error`: a
    /// too-large `n`, or a host that cannot satisfy the reservation, yields a
    /// clean [`HeapFault::OutOfMemory`] for the caller to turn into a program
    /// panic / error arm.
    pub fn alloc_many(&mut self, init: Value, n: usize) -> Result<Ptr, HeapFault> {
        if n > MAX_ALLOC_ELEMS {
            return Err(HeapFault::OutOfMemory);
        }
        let mut data: Vec<Value> = Vec::new();
        data.try_reserve_exact(n)
            .map_err(|_| HeapFault::OutOfMemory)?;
        data.resize(n, init);
        let cell = self.cells.len() as u32;
        self.cells.push(Cell {
            live: true,
            data: CellData::Many(data),
        });
        Ok(Ptr { cell, offset: 0 })
    }

    /// Allocates a multi-value cell holding an existing run of values, returning a
    /// pointer to its first element. Used to give a by-value array a real heap
    /// home when a slice/`.ptr` is taken from it (the element count is already
    /// bounded by the source array, so no extra cap is needed).
    pub fn alloc_run(&mut self, values: Vec<Value>) -> Ptr {
        let cell = self.cells.len() as u32;
        self.cells.push(Cell {
            live: true,
            data: CellData::Many(values),
        });
        Ptr { cell, offset: 0 }
    }

    /// Marks the cell behind `ptr` dead (idempotent). A double free is tolerated
    /// (it is a no-op), since the leak/escape pass already rejects the obvious
    /// misuse shapes.
    pub fn free(&mut self, ptr: Ptr) {
        if let Some(cell) = self.cells.get_mut(ptr.cell as usize) {
            cell.live = false;
        }
    }

    /// Reallocates the run behind `ptr` to `n` elements, returning a pointer to
    /// the new cell. The contents are preserved up to `min(old_len, n)`; any new
    /// tail is `init`. The **old cell is freed**, so any stale pointer/slice into
    /// it trips the use-after-free check on its next access — matching the spec's
    /// "after a successful `realloc`, the old slice is invalid".
    ///
    /// As with [`alloc_many`](Self::alloc_many), the element count is validated
    /// against [`MAX_ALLOC_ELEMS`] and the backing run is grown fallibly, so an
    /// over-large request is a clean [`HeapFault::OutOfMemory`] rather than an
    /// aborting global-allocator failure.
    pub fn realloc(&mut self, ptr: Ptr, n: usize, init: Value) -> Result<Ptr, HeapFault> {
        if n > MAX_ALLOC_ELEMS {
            return Err(HeapFault::OutOfMemory);
        }
        // Snapshot the old contents (an empty run for the null/empty slice).
        let old: Vec<Value> = match self.cells.get(ptr.cell as usize) {
            Some(cell) => match &cell.data {
                CellData::One(v) => vec![v.clone()],
                CellData::Many(vs) => vs.clone(),
            },
            None => Vec::new(),
        };
        let mut data: Vec<Value> = Vec::new();
        data.try_reserve_exact(n)
            .map_err(|_| HeapFault::OutOfMemory)?;
        let keep = old.len().min(n);
        data.extend(old.into_iter().take(keep));
        data.resize(n, init);
        let cell = self.cells.len() as u32;
        self.cells.push(Cell {
            live: true,
            data: CellData::Many(data),
        });
        // Invalidate the old allocation (a no-op for the null/empty slice).
        if !ptr.is_null() {
            self.free(ptr);
        }
        Ok(Ptr { cell, offset: 0 })
    }

    /// The number of elements in the allocation behind `ptr` (the cell length for
    /// a `Many` cell, `1` for a `One` cell). Used for slice length recovery.
    pub fn len_of(&self, ptr: Ptr) -> Option<usize> {
        let cell = self.cells.get(ptr.cell as usize)?;
        Some(match &cell.data {
            CellData::One(Value::Array(a)) => a.len(),
            CellData::One(_) => 1,
            CellData::Many(v) => v.len(),
        })
    }

    /// Loads the value at `ptr` (following the offset for a multi-element cell).
    pub fn load(&self, ptr: Ptr) -> Result<Value, HeapFault> {
        if ptr.is_null() {
            return Err(HeapFault::NullPointer);
        }
        let cell = self
            .cells
            .get(ptr.cell as usize)
            .ok_or(HeapFault::NullPointer)?;
        if !cell.live && !self.ignore_liveness {
            return Err(HeapFault::UseAfterFree);
        }
        match &cell.data {
            CellData::One(v) => Ok(v.clone()),
            CellData::Many(vs) => vs.get(ptr.offset).cloned().ok_or(HeapFault::OutOfRange),
        }
    }

    /// Stores `value` at `ptr` (following the offset for a multi-element cell).
    pub fn store(&mut self, ptr: Ptr, value: Value) -> Result<(), HeapFault> {
        if ptr.is_null() {
            return Err(HeapFault::NullPointer);
        }
        let ignore = self.ignore_liveness;
        let cell = self
            .cells
            .get_mut(ptr.cell as usize)
            .ok_or(HeapFault::NullPointer)?;
        if !cell.live && !ignore {
            return Err(HeapFault::UseAfterFree);
        }
        match &mut cell.data {
            CellData::One(slot) => {
                *slot = value;
                Ok(())
            }
            CellData::Many(vs) => {
                let slot = vs.get_mut(ptr.offset).ok_or(HeapFault::OutOfRange)?;
                *slot = value;
                Ok(())
            }
        }
    }

    /// Loads the `i`-th element of the allocation behind `ptr` (i.e. `ptr[i]`,
    /// honouring `ptr.offset` as the base). Used for slice/array indexing through
    /// a heap pointer.
    ///
    /// A `CellData::One(Value::Array(..))` cell is treated as an indexable run:
    /// the element offset (`ptr.offset + i`) indexes *into the inner array*. This
    /// is what lets a [`FixedBufferAllocator`](crate::vm) carve non-aliasing
    /// sub-views out of one boxed `&storage` array — each sub-view is a slice
    /// whose `ptr.offset` selects a distinct window of the backing array, so
    /// `a[0]` and `b[0]` no longer clobber the same scalar. (The whole-cell
    /// [`load`](Self::load) accessor is unchanged, so `&array` used as a
    /// pointer-to-array still yields the whole array.)
    pub fn load_index(&self, ptr: Ptr, i: usize) -> Result<Value, HeapFault> {
        if ptr.is_null() {
            return Err(HeapFault::NullPointer);
        }
        let cell = self
            .cells
            .get(ptr.cell as usize)
            .ok_or(HeapFault::NullPointer)?;
        if !cell.live && !self.ignore_liveness {
            return Err(HeapFault::UseAfterFree);
        }
        let off = ptr.offset + i;
        match &cell.data {
            CellData::One(Value::Array(a)) => a.get(off).cloned().ok_or(HeapFault::OutOfRange),
            CellData::One(v) if off == 0 => Ok(v.clone()),
            CellData::One(_) => Err(HeapFault::OutOfRange),
            CellData::Many(vs) => vs.get(off).cloned().ok_or(HeapFault::OutOfRange),
        }
    }

    /// Stores into the `i`-th element of the allocation behind `ptr`. As with
    /// [`load_index`](Self::load_index), a `CellData::One(Value::Array(..))` cell
    /// is indexed *into the inner array*, so FBA sub-views write to disjoint
    /// windows of the shared backing array.
    pub fn store_index(&mut self, ptr: Ptr, i: usize, value: Value) -> Result<(), HeapFault> {
        if ptr.is_null() {
            return Err(HeapFault::NullPointer);
        }
        let ignore = self.ignore_liveness;
        let cell = self
            .cells
            .get_mut(ptr.cell as usize)
            .ok_or(HeapFault::NullPointer)?;
        if !cell.live && !ignore {
            return Err(HeapFault::UseAfterFree);
        }
        let off = ptr.offset + i;
        match &mut cell.data {
            CellData::One(Value::Array(a)) => {
                let slot = std::rc::Rc::make_mut(a)
                    .get_mut(off)
                    .ok_or(HeapFault::OutOfRange)?;
                *slot = value;
                Ok(())
            }
            CellData::One(slot) if off == 0 => {
                *slot = value;
                Ok(())
            }
            CellData::One(_) => Err(HeapFault::OutOfRange),
            CellData::Many(vs) => {
                let slot = vs.get_mut(off).ok_or(HeapFault::OutOfRange)?;
                *slot = value;
                Ok(())
            }
        }
    }
}
