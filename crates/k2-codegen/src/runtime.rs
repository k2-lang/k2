//! The native `*System` runtime: hand-written machine-code support routines.
//!
//! The VM realizes the heap and the `*System` capability floor (`alloc`/`free`/
//! `realloc`/`create`/`destroy`, the handle-based allocator registry with leak /
//! double-free detection, the deterministic clock and PRNG) in Rust. The native
//! backend must produce *byte-identical* observable behavior with **no libc and
//! no external crates** — so each of those operations is emitted here as a small
//! routine of raw x86-64 machine code, appended to `.text` after the user
//! functions and reached from the lowering through a [`FixupKind::Runtime`] call
//! relocation (see [`crate::encode`] / [`crate::link`]).
//!
//! ## The model (matching `k2_vm`)
//!
//! The allocator stays **handle-based**, exactly like the VM (`AllocatorKind` +
//! `AllocatorState`): `@allocId(kind,…)` mints a handle id, the std allocators in
//! `std.k2` are thin k2 wrappers over the leaf intrinsics, and every op dispatches
//! on the handle. Here each handle is one slot in a **registry** that lives in the
//! writable state segment (a `.bss`-like third `PT_LOAD`, see [`crate::elf`]); the
//! slot records the allocator kind plus its per-kind bookkeeping. Allocation comes
//! from anonymous `mmap`, one region per allocation, each prefixed by a header that
//! carries its lengths, owner handle, a liveness flag, and an intrusive `next`
//! link threading the handle's live blocks (for arena bulk-free and the GPA's
//! `deinit` reclamation).
//!
//! ## The state segment
//!
//! ```text
//!   off 0    reg_next   : u64   (next free handle id; 1 after _start inits slot 0)
//!   off 8    clock_nanos: u64   (deterministic monotonic counter, advanced by sleep)
//!   off 16   rng_state  : u64   (splitmix64 state; seeded by _start)
//!   off 64   registry[REG_MAX] : REG_SLOT bytes each
//! ```
//!
//! Each registry slot:
//!
//! ```text
//!   +0   kind       : u32   (0=Default 1=Gpa 2=Arena 3=FixedBuffer)
//!   +4   live_count : u32   (handed-out-but-not-freed blocks; the leak counter)
//!   +8   fba_buf    : u64   (FixedBuffer backing buffer base)
//!   +16  fba_cap    : u64   (FixedBuffer capacity in bytes)
//!   +24  fba_off    : u64   (FixedBuffer bump offset in bytes)
//!   +32  live_head  : u64   (head of the intrusive live-block list)
//! ```
//!
//! Each heap block's mmap region is laid out as a header then the payload:
//!
//! ```text
//!   +0   magic       : u64   (BLOCK_MAGIC — distinguishes a real heap block)
//!   +8   total_len   : u64   (the rounded mmap length, for munmap)
//!   +16  payload_len : u64   (the requested byte count, for realloc copy)
//!   +24  owner       : u32   (the allocating handle id)
//!   +28  live        : u32   (1 live, 0 freed — drives double-free detection)
//!   +32  next        : u64   (intrusive live-list link)
//!   +4096 payload .......... (the address handed back to k2 code)
//! ```
//!
//! The header occupies a **full page** (`HDR == PAGE`); the live fields use only
//! the first 40 bytes, but page-padding the header puts the payload on its own
//! page boundary. That is what lets `free` `mprotect` the *whole* payload span
//! `PROT_NONE` — so any later read (even of byte 0, even of a one-page block)
//! faults — while the header page survives for a subsequent double-free probe.
//!
//! ## Narrowings vs the VM (documented; see `CHANGELOG.md`)
//!
//! * **Use-after-free** in Debug traps via `SIGSEGV` (the freed payload is
//!   page-isolated and `mprotect`-ed `PROT_NONE` in full) rather than the VM's
//!   clean `use after free` panic; `k2c run-native` maps the signal death to `139`
//!   while the VM exits `134`. The fault is reliable for *every* freed payload —
//!   any offset, any size — because the payload sits on its own page(s); only the
//!   exit *code* differs (`139` native vs `134` VM), and the acceptance corpus
//!   never commits a UAF on its success path, so corpus exit codes still match.
//!   Leak and double-free detection are full-fidelity (clean `bool` / clean `134`).
//! * The heap is one `mmap` region per allocation with no free-list reuse — simple
//!   and auditable, exactly the VM's "independent liveness per allocation" shape.

use crate::encode::Asm;
use crate::reg::Gpr;

/// One emittable runtime support routine. Each variant is a self-contained block
/// of machine code appended to `.text`; the lowering reaches it via
/// [`crate::encode::FixupKind::Runtime`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RuntimeFn {
    /// `__k2_alloc(handle=rdi, n=rsi, elem_size=rdx) -> ptr in rax` (0 on FBA
    /// exhaustion, so the caller writes the `OutOfMemory` error arm).
    Alloc,
    /// `__k2_free(handle=rdi, ptr=rsi, checks_off=rdx)`.
    Free,
    /// `__k2_realloc(handle=rdi, old_ptr=rsi, new_n=rdx, elem_size=rcx,
    /// checks_off=r8) -> new_ptr in rax`.
    Realloc,
    /// `__k2_alloc_id(kind=rdi, fba_buf=rsi, fba_cap=rdx) -> handle in rax`.
    AllocId,
    /// `__k2_gpa_deinit(handle=rdi, checks_off=rsi) -> bool in rax`.
    GpaDeinit,
    /// `__k2_arena_deinit(handle=rdi)`.
    ArenaDeinit,
}

impl RuntimeFn {
    /// Every runtime routine, for the link pass to emit + place.
    pub const ALL: [RuntimeFn; 6] = [
        RuntimeFn::Alloc,
        RuntimeFn::Free,
        RuntimeFn::Realloc,
        RuntimeFn::AllocId,
        RuntimeFn::GpaDeinit,
        RuntimeFn::ArenaDeinit,
    ];
}

// ---- State-segment layout (byte offsets within the writable PT_LOAD). ----

/// `reg_next`: the next free handle id (u64). Initialized to 1 by `_start` (slot
/// 0 is the always-present default allocator).
pub const ST_REG_NEXT: u32 = 0;
/// `clock_nanos`: the deterministic monotonic counter (u64), advanced by sleep.
pub const ST_CLOCK_NANOS: u32 = 8;
/// `rng_state`: the splitmix64 PRNG state (u64), seeded by `_start`.
pub const ST_RNG_STATE: u32 = 16;
/// The registry base (first slot). Slot 0 is the default allocator.
pub const ST_REGISTRY: u32 = 64;

/// The byte size of one registry slot.
const REG_SLOT: i64 = 48;
/// The maximum number of allocator handles (registry slots).
pub const REG_MAX: u32 = 256;

// Registry-slot field offsets.
const RS_KIND: i32 = 0;
const RS_LIVE_COUNT: i32 = 4;
const RS_FBA_BUF: i32 = 8;
const RS_FBA_CAP: i32 = 16;
const RS_FBA_OFF: i32 = 24;
const RS_LIVE_HEAD: i32 = 32;

// Allocator-kind tags (match the VM's `@allocId` numeric kinds via `kind_tag`).
const KIND_DEFAULT: i64 = 0;
const KIND_GPA: i64 = 1;
const KIND_ARENA: i64 = 2;
const KIND_FIXED: i64 = 3;

// Heap-block header layout (bytes).
/// The total header size; the payload begins here. The header occupies a FULL
/// page so the payload starts on its own page boundary: that page isolation is
/// what lets `free` `mprotect` the *entire* payload — including its first byte —
/// `PROT_NONE` for full-fidelity use-after-free trapping, while the header page
/// (with the magic + live flag) stays readable for later double-free probing. The
/// header fields themselves only occupy the first 40 bytes; the rest of the page
/// is slack. (The VM models no native layout, so this padding is invisible to the
/// `native == VM` observable behavior; it only costs one extra page per block.)
pub const HDR: i64 = PAGE;
const H_MAGIC: i32 = 0;
const H_TOTAL_LEN: i32 = 8;
const H_PAYLOAD_LEN: i32 = 16;
const H_OWNER: i32 = 24;
const H_LIVE: i32 = 28;
const H_NEXT: i32 = 32;

/// The header magic distinguishing a real mmap heap block from any other pointer
/// (a stack/rodata/FBA address) when a GPA `free` validates its operand. Spells
/// "k2LOCHDR" in ASCII, an arbitrary but recognizable 64-bit tag.
const BLOCK_MAGIC: i64 = 0x6B32_4C4F_4348_4452u64 as i64;

// Linux x86-64 syscall numbers used by the runtime.
const SYS_MMAP: i64 = 9;
const SYS_MUNMAP: i64 = 11;
const SYS_MPROTECT: i64 = 10;
const SYS_EXIT: i64 = 60;

// mmap / mprotect flags.
const PROT_RW: i64 = 0x3; // PROT_READ | PROT_WRITE
const PROT_NONE: i64 = 0x0;
const MAP_PRIVATE_ANON: i64 = 0x22; // MAP_PRIVATE | MAP_ANONYMOUS
const PAGE: i64 = 0x1000;

/// The process exit code a runtime trap (double / invalid free) maps to, matching
/// the VM's clean-panic `134`.
const TRAP_EXIT: i64 = 134;

/// The total byte size of the writable state segment: the fixed header (reg_next /
/// clock / RNG, padded to the registry base) plus `REG_MAX` registry slots.
pub fn state_segment_size() -> u64 {
    ST_REGISTRY as u64 + REG_MAX as u64 * REG_SLOT as u64
}

/// Maps the std `@allocId` numeric kind (0=Default 1=GPA 2=Arena 3=FixedBuffer,
/// 5=Testing) to a registry kind tag. GPA and testing share the tracker, exactly
/// as the VM's `register_allocator` folds `1 | 5 => Gpa`.
pub fn kind_tag(alloc_id_kind: usize) -> i64 {
    match alloc_id_kind {
        1 | 5 => KIND_GPA,
        2 => KIND_ARENA,
        3 => KIND_FIXED,
        _ => KIND_DEFAULT,
    }
}

/// Emits the machine code for one runtime routine into a fresh [`Asm`], returning
/// the finished bytes plus the surviving fixups (the routines reference the state
/// segment via [`FixupKind::State`] holes; they make no cross-function calls, so
/// the only fixups are `State`). The link pass appends these after the user code.
pub fn emit(rt: RuntimeFn) -> Asm {
    let mut a = Asm::new();
    a.reserve_labels(0);
    match rt {
        RuntimeFn::Alloc => emit_alloc(&mut a),
        RuntimeFn::Free => emit_free(&mut a),
        RuntimeFn::Realloc => emit_realloc(&mut a),
        RuntimeFn::AllocId => emit_alloc_id(&mut a),
        RuntimeFn::GpaDeinit => emit_gpa_deinit(&mut a),
        RuntimeFn::ArenaDeinit => emit_arena_deinit(&mut a),
    }
    a
}

// ---------------------------------------------------------------------
//  Small inline building blocks
// ---------------------------------------------------------------------

/// Loads the absolute address of registry slot `handle` (in `handle_reg`) into
/// `dst`. `dst` and `scratch` must differ from `handle_reg`. Computes
/// `state_base(ST_REGISTRY) + handle * REG_SLOT`.
fn reg_slot_addr(a: &mut Asm, dst: Gpr, handle_reg: Gpr, scratch: Gpr) {
    // scratch = handle * REG_SLOT
    a.mov_rr(scratch, handle_reg);
    a.imul_rri(scratch, scratch, REG_SLOT as i32);
    // dst = &registry[0]
    a.mov_ri_state(dst, ST_REGISTRY);
    a.add_rr(dst, scratch);
}

// ---------------------------------------------------------------------
//  __k2_alloc(handle=rdi, n=rsi, elem_size=rdx) -> ptr in rax
// ---------------------------------------------------------------------

fn emit_alloc(a: &mut Asm) {
    // bytes = n * elem_size  -> r8
    a.mov_rr(Gpr::R8, Gpr::Rsi);
    a.imul_rr(Gpr::R8, Gpr::Rdx); // r8 = requested payload bytes

    // Slot address -> r9 (handle stays in rdi).
    reg_slot_addr(a, Gpr::R9, Gpr::Rdi, Gpr::Rcx);

    // --- FixedBuffer fast path: kind == 3 bumps the caller buffer. ---
    a.mov_load32_mem(Gpr::Rcx, Gpr::R9, RS_KIND);
    a.cmp_ri(Gpr::Rcx, KIND_FIXED as i32);
    let not_fba = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, not_fba);
    emit_fba_alloc(a); // returns from the routine on both branches
    a.bind_local(not_fba);

    // The mmap syscall clobbers every caller-saved register including r9 (the 6th
    // mmap arg = offset), r11, and rcx. Preserve the slot address (r9) and the
    // handle (rdi) on the stack across the syscall. Keep 16-byte alignment with a
    // 32-byte frame holding [r9, rdi, r8(bytes), pad].
    a.sub_rsp_imm(32);
    a.mov_store_mem(Gpr::Rsp, 0, Gpr::R9); // slot addr
    a.mov_store_mem(Gpr::Rsp, 8, Gpr::Rdi); // handle
    a.mov_store_mem(Gpr::Rsp, 16, Gpr::R8); // requested bytes

    // --- mmap-backed allocation. ---
    // total = round_up(bytes + HDR, PAGE)  -> rsi (the mmap length)
    a.mov_rr(Gpr::Rsi, Gpr::R8);
    a.add_ri(Gpr::Rsi, HDR as i32);
    a.add_ri(Gpr::Rsi, (PAGE - 1) as i32);
    // rsi &= ~(PAGE-1): emit `and rsi, ~0xFFF` (a sign-extended imm32 of -4096).
    a.and_ri(Gpr::Rsi, !(PAGE as i32 - 1));

    // mmap(addr=0, len=rsi, prot=RW, flags=PRIVATE|ANON, fd=-1, off=0).
    emit_mmap(a); // region -> rax (kernel error: rax in [-4095,-1])

    // Restore the saved values (rsi still holds the rounded mmap length).
    a.mov_load_mem(Gpr::R9, Gpr::Rsp, 0); // slot addr
    a.mov_load_mem(Gpr::Rdi, Gpr::Rsp, 8); // handle
    a.mov_load_mem(Gpr::R8, Gpr::Rsp, 16); // requested bytes
    a.add_rsp_imm(32);

    // On mmap failure, trap as a clean OOM panic (the corpus never hits this).
    a.test_rr(Gpr::Rax, Gpr::Rax);
    let ok = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ns, ok); // rax >= 0 => success
                                            // rax in [-4095,-1]: treat as failure.
    emit_oom_trap(a);
    a.bind_local(ok);

    // rax = region base. Fill the header.
    // magic
    a.mov_ri(Gpr::Rcx, BLOCK_MAGIC);
    a.mov_store_mem(Gpr::Rax, H_MAGIC, Gpr::Rcx);
    // total_len (rsi)
    a.mov_store_mem(Gpr::Rax, H_TOTAL_LEN, Gpr::Rsi);
    // payload_len (r8)
    a.mov_store_mem(Gpr::Rax, H_PAYLOAD_LEN, Gpr::R8);
    // owner (rdi, low 32)
    a.mov_store32_mem(Gpr::Rax, H_OWNER, Gpr::Rdi);
    // live = 1
    a.mov_ri(Gpr::Rcx, 1);
    a.mov_store32_mem(Gpr::Rax, H_LIVE, Gpr::Rcx);

    // Track into the handle's live list when kind is GPA or Arena.
    // (Default keeps no bookkeeping; FBA already returned.)
    emit_track(a, Gpr::R9, Gpr::Rax);

    // Return payload = region + HDR.
    a.add_ri(Gpr::Rax, HDR as i32);
    a.ret();
}

/// FBA allocation: bump `fba_off` within `[fba_buf, fba_buf+fba_cap)`. Inputs:
/// `r9` = slot address, `r8` = requested bytes. Returns from the routine: on
/// success `rax = fba_buf + old_off`; on exhaustion `rax = 0` (the caller writes
/// the `OutOfMemory` error arm). No header, no tracking — FBA `free` is a no-op.
fn emit_fba_alloc(a: &mut Asm) {
    // rcx = off, rdx = cap, rsi = buf
    a.mov_load_mem(Gpr::Rcx, Gpr::R9, RS_FBA_OFF);
    a.mov_load_mem(Gpr::Rdx, Gpr::R9, RS_FBA_CAP);
    a.mov_load_mem(Gpr::Rsi, Gpr::R9, RS_FBA_BUF);
    // new_off = off + bytes
    a.mov_rr(Gpr::Rax, Gpr::Rcx);
    a.add_rr(Gpr::Rax, Gpr::R8);
    // if new_off > cap -> exhausted (return 0).
    a.cmp_rr(Gpr::Rax, Gpr::Rdx);
    let exhausted = a.new_local_label();
    a.jcc_local(crate::encode::Cc::A, exhausted);
    // ptr = buf + old_off
    a.mov_rr(Gpr::Rdx, Gpr::Rsi);
    a.add_rr(Gpr::Rdx, Gpr::Rcx);
    // store new_off
    a.mov_store_mem(Gpr::R9, RS_FBA_OFF, Gpr::Rax);
    a.mov_rr(Gpr::Rax, Gpr::Rdx);
    a.ret();
    a.bind_local(exhausted);
    a.mov_ri(Gpr::Rax, 0);
    a.ret();
}

/// Pushes block `hdr` (in `hdr_reg`) onto the live list of the slot at `slot_reg`
/// and bumps `live_count`, but ONLY for GPA/Arena kinds. Clobbers rcx, rdx.
/// (The `next` link is written for every kind so a later list walk is safe, but
/// the head/count only advance for tracked kinds.)
fn emit_track(a: &mut Asm, slot_reg: Gpr, hdr_reg: Gpr) {
    a.mov_load32_mem(Gpr::Rcx, slot_reg, RS_KIND);
    // Only GPA (1) or Arena (2) track. Skip otherwise.
    let done = a.new_local_label();
    // if kind != GPA and kind != Arena, skip.
    a.cmp_ri(Gpr::Rcx, KIND_GPA as i32);
    let do_track = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, do_track);
    a.cmp_ri(Gpr::Rcx, KIND_ARENA as i32);
    a.jcc_local(crate::encode::Cc::Ne, done);
    a.bind_local(do_track);
    // hdr.next = slot.live_head; slot.live_head = hdr.
    a.mov_load_mem(Gpr::Rdx, slot_reg, RS_LIVE_HEAD);
    a.mov_store_mem(hdr_reg, H_NEXT, Gpr::Rdx);
    a.mov_store_mem(slot_reg, RS_LIVE_HEAD, hdr_reg);
    // live_count += 1.
    a.mov_load32_mem(Gpr::Rcx, slot_reg, RS_LIVE_COUNT);
    a.add_ri(Gpr::Rcx, 1);
    a.mov_store32_mem(slot_reg, RS_LIVE_COUNT, Gpr::Rcx);
    a.bind_local(done);
}

/// Splices block `hdr` (in `hdr_reg`) out of the slot's intrusive `live_head`
/// singly-linked list, patching the predecessor's `H_NEXT` link (or
/// `RS_LIVE_HEAD` if `hdr` is the head). A no-op if `hdr` is not on the list.
/// Clobbers rax/rcx/rdx; preserves `slot_reg` and `hdr_reg`.
///
/// This is the native mirror of the VM's `st.live.retain(|&c| c != ptr.cell)`
/// (`alloc_free` / `retrack_realloc` in `vm.rs`): once a tracked block is freed
/// (directly, or as the old block of a `realloc`) it must leave the live list so
/// the single `deinit` reclamation walk neither double-frees it nor — fatally —
/// dereferences a block that an eager `munmap` already returned to the kernel.
/// Keeping this invariant is what lets every block on `live_head` stay MAPPED
/// until `emit_free_list_walk` reclaims it.
fn emit_unlink_block(a: &mut Asm, slot_reg: Gpr, hdr_reg: Gpr) {
    // cur (rax) = slot.live_head.
    a.mov_load_mem(Gpr::Rax, slot_reg, RS_LIVE_HEAD);
    let done = a.new_local_label();
    // Empty list: nothing to do.
    a.test_rr(Gpr::Rax, Gpr::Rax);
    a.jcc_local(crate::encode::Cc::E, done);
    // Head match: slot.live_head = hdr.next.
    a.cmp_rr(Gpr::Rax, hdr_reg);
    let not_head = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, not_head);
    a.mov_load_mem(Gpr::Rdx, hdr_reg, H_NEXT);
    a.mov_store_mem(slot_reg, RS_LIVE_HEAD, Gpr::Rdx);
    a.jmp_local(done);
    a.bind_local(not_head);
    // Walk: while (cur.next != 0) { if (cur.next == hdr) { cur.next = hdr.next;
    // break; } cur = cur.next; }.
    let loop_top = a.new_local_label();
    a.bind_local(loop_top);
    // next (rcx) = cur.next.
    a.mov_load_mem(Gpr::Rcx, Gpr::Rax, H_NEXT);
    a.test_rr(Gpr::Rcx, Gpr::Rcx);
    a.jcc_local(crate::encode::Cc::E, done); // end of list: not found.
    a.cmp_rr(Gpr::Rcx, hdr_reg);
    let found = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, found);
    // cur = next; continue.
    a.mov_rr(Gpr::Rax, Gpr::Rcx);
    a.jmp_local(loop_top);
    a.bind_local(found);
    // cur.next = hdr.next.
    a.mov_load_mem(Gpr::Rdx, hdr_reg, H_NEXT);
    a.mov_store_mem(Gpr::Rax, H_NEXT, Gpr::Rdx);
    a.bind_local(done);
}

// ---------------------------------------------------------------------
//  __k2_free(handle=rdi, ptr=rsi, checks_off=rdx)
// ---------------------------------------------------------------------

fn emit_free(a: &mut Asm) {
    // ptr == 0 (null / empty slice): benign no-op.
    a.test_rr(Gpr::Rsi, Gpr::Rsi);
    let not_null = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, not_null);
    a.ret();
    a.bind_local(not_null);

    // Slot address -> r9.
    reg_slot_addr(a, Gpr::R9, Gpr::Rdi, Gpr::Rcx);
    a.mov_load32_mem(Gpr::Rcx, Gpr::R9, RS_KIND);

    // Arena / FixedBuffer: free is a no-op (bulk / reset only).
    a.cmp_ri(Gpr::Rcx, KIND_ARENA as i32);
    let noop = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, noop);
    a.cmp_ri(Gpr::Rcx, KIND_FIXED as i32);
    a.jcc_local(crate::encode::Cc::E, noop);

    // GPA with checks on: the validated path.
    a.cmp_ri(Gpr::Rcx, KIND_GPA as i32);
    let plain = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, plain);
    // checks_off (rdx) != 0 -> plain free.
    a.test_rr(Gpr::Rdx, Gpr::Rdx);
    a.jcc_local(crate::encode::Cc::Ne, plain);
    emit_gpa_free(a, Gpr::R9); // header in rsi-HDR; validates + munmaps-on-deinit
                               // emit_gpa_free returns from the routine.

    a.bind_local(plain);
    // Default / checks-off: plain free. hdr = ptr - HDR; munmap(hdr, total_len).
    emit_plain_munmap(a);
    a.ret();

    a.bind_local(noop);
    a.ret();
}

/// The checked GPA free. `ptr` in rsi, `slot` in `slot_reg` (r9). Validates the
/// block magic + owner (invalid free trap), the live flag (double-free trap), then
/// sets `live=0`, decrements `live_count`, **unlinks the block from the live
/// list**, and `mprotect`s the whole payload `PROT_NONE` (so any later access
/// faults = native UAF). The region is intentionally **left mapped** — the header
/// page survives for a subsequent double-free probe, and since the block is no
/// longer on `live_head` the `deinit` walk never touches it (the kernel reclaims
/// it at process exit, mirroring the VM keeping a freed cell's backing until then).
/// Returns from the routine.
fn emit_gpa_free(a: &mut Asm, slot_reg: Gpr) {
    // hdr = ptr - HDR -> r10.
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));

    // Validate magic.
    a.mov_load_mem(Gpr::Rax, Gpr::R10, H_MAGIC);
    a.mov_ri(Gpr::Rcx, BLOCK_MAGIC);
    a.cmp_rr(Gpr::Rax, Gpr::Rcx);
    let bad = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, bad);
    // Validate owner == handle (rdi).
    a.mov_load32_mem(Gpr::Rax, Gpr::R10, H_OWNER);
    a.cmp_rr(Gpr::Rax, Gpr::Rdi);
    a.jcc_local(crate::encode::Cc::Ne, bad);
    // Double-free: live == 0.
    a.mov_load32_mem(Gpr::Rax, Gpr::R10, H_LIVE);
    a.test_rr(Gpr::Rax, Gpr::Rax);
    let dbl = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, dbl);

    // live = 0.
    a.mov_ri(Gpr::Rax, 0);
    a.mov_store32_mem(Gpr::R10, H_LIVE, Gpr::Rax);
    // live_count -= 1.
    a.mov_load32_mem(Gpr::Rax, slot_reg, RS_LIVE_COUNT);
    a.add_ri(Gpr::Rax, -1);
    a.mov_store32_mem(slot_reg, RS_LIVE_COUNT, Gpr::Rax);
    // Unlink from the live list (clobbers rax/rcx/rdx; preserves r9/r10) so the
    // block no longer feeds the `deinit` reclamation walk — it is reclaimed here
    // (well, kept MAPPED for double-free/UAF probing until process exit), never
    // double-touched at teardown. This is the unlink the doc comment promises.
    emit_unlink_block(a, slot_reg, Gpr::R10);

    // mprotect the *entire* payload span PROT_NONE so any later read or write —
    // at any offset, for any block size — faults (full-fidelity native UAF). The
    // payload sits on its own page boundary (`HDR == PAGE`), so the protected
    // region is exactly `[hdr + HDR, hdr + total_len)`: the header page (with the
    // magic + live flag) stays readable for a subsequent double-free probe, while
    // the whole payload becomes inaccessible. A zero-byte payload (total_len ==
    // PAGE) has no payload page; skip it. addr = hdr + HDR, len = total_len - HDR.
    a.mov_load_mem(Gpr::Rdx, Gpr::R10, H_TOTAL_LEN);
    a.cmp_ri(Gpr::Rdx, (HDR + PAGE) as i32);
    let skip_prot = a.new_local_label();
    a.jcc_local(crate::encode::Cc::B, skip_prot);
    a.mov_rr(Gpr::Rdi, Gpr::R10);
    a.add_ri(Gpr::Rdi, HDR as i32);
    a.mov_rr(Gpr::Rsi, Gpr::Rdx);
    a.add_ri(Gpr::Rsi, -(HDR as i32));
    a.mov_ri(Gpr::Rdx, PROT_NONE);
    a.mov_ri(Gpr::Rax, SYS_MPROTECT);
    a.syscall();
    a.bind_local(skip_prot);
    a.ret();

    a.bind_local(dbl);
    emit_trap(a, b"panic: double free detected\n");
    a.bind_local(bad);
    emit_trap(
        a,
        b"panic: invalid free: pointer was not allocated by this allocator\n",
    );
}

/// Default/checks-off plain free: hdr = ptr(rsi) - HDR; munmap(hdr, total_len).
/// Only munmaps a real mmap block (magic-validated); a foreign pointer is ignored.
fn emit_plain_munmap(a: &mut Asm) {
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));
    a.mov_load_mem(Gpr::Rax, Gpr::R10, H_MAGIC);
    a.mov_ri(Gpr::Rcx, BLOCK_MAGIC);
    a.cmp_rr(Gpr::Rax, Gpr::Rcx);
    let done = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, done);
    // munmap(hdr, total_len).
    a.mov_load_mem(Gpr::Rsi, Gpr::R10, H_TOTAL_LEN);
    a.mov_rr(Gpr::Rdi, Gpr::R10);
    a.mov_ri(Gpr::Rax, SYS_MUNMAP);
    a.syscall();
    a.bind_local(done);
}

// ---------------------------------------------------------------------
//  __k2_realloc(handle=rdi, old_ptr=rsi, new_n=rdx, elem_size=rcx,
//               checks_off=r8) -> new_ptr in rax
// ---------------------------------------------------------------------

fn emit_realloc(a: &mut Asm) {
    // Inputs: handle=rdi, old_ptr=rsi, new_n=rdx, elem_size=rcx, checks_off=r8.
    // We use a small stack frame to keep the four values we still need after the
    // (register-clobbering) alloc + rep-movsb: [handle, old_ptr, checks_off, new].
    // Reserve 32 bytes (16-aligned). Slots relative to rsp:
    //   [rsp+0]=handle  [rsp+8]=old_ptr  [rsp+16]=checks_off  [rsp+24]=new_ptr
    a.sub_rsp_imm(32);
    a.mov_store_mem(Gpr::Rsp, 0, Gpr::Rdi);
    a.mov_store_mem(Gpr::Rsp, 8, Gpr::Rsi);
    a.mov_store_mem(Gpr::Rsp, 16, Gpr::R8);

    // alloc-inline(handle=rdi, n=new_n, elem_size). rdi already = handle.
    a.mov_rr(Gpr::Rsi, Gpr::Rdx); // n
    a.mov_rr(Gpr::Rdx, Gpr::Rcx); // elem_size
    emit_alloc_inline(a); // new payload -> rax
    a.mov_store_mem(Gpr::Rsp, 24, Gpr::Rax); // save new_ptr

    // If old_ptr == 0: return new (no copy/free).
    a.mov_load_mem(Gpr::Rsi, Gpr::Rsp, 8); // old_ptr
    a.test_rr(Gpr::Rsi, Gpr::Rsi);
    let finish = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, finish);
    // If new_ptr == 0 (OOM): the saved new is 0; return it.
    a.mov_load_mem(Gpr::Rax, Gpr::Rsp, 24);
    a.test_rr(Gpr::Rax, Gpr::Rax);
    a.jcc_local(crate::encode::Cc::E, finish);

    // Copy min(old.payload_len, new.payload_len) from old payload -> new payload.
    // old hdr = old_ptr - HDR.
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));
    a.mov_load_mem(Gpr::Rcx, Gpr::R10, H_PAYLOAD_LEN); // old payload_len
    a.mov_load_mem(Gpr::R11, Gpr::Rsp, 24); // new_ptr
    a.mov_rr(Gpr::Rax, Gpr::R11);
    a.add_ri(Gpr::Rax, -(HDR as i32)); // new hdr
    a.mov_load_mem(Gpr::Rax, Gpr::Rax, H_PAYLOAD_LEN); // new payload_len
                                                       // rcx = min(old, new).
    a.cmp_rr(Gpr::Rcx, Gpr::Rax);
    let have_min = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Be, have_min);
    a.mov_rr(Gpr::Rcx, Gpr::Rax);
    a.bind_local(have_min);
    // rep movsb: rsi=old payload (set above), rdi=new payload, rcx=count.
    a.mov_rr(Gpr::Rdi, Gpr::R11);
    a.rep_movsb();

    // free-inline(handle, old_ptr, checks_off).
    a.mov_load_mem(Gpr::Rdi, Gpr::Rsp, 0); // handle
    a.mov_load_mem(Gpr::Rsi, Gpr::Rsp, 8); // old_ptr
    a.mov_load_mem(Gpr::Rdx, Gpr::Rsp, 16); // checks_off
    emit_free_inline(a);

    a.bind_local(finish);
    a.mov_load_mem(Gpr::Rax, Gpr::Rsp, 24); // new_ptr
    a.add_rsp_imm(32);
    a.ret();
}

/// The mmap-backed allocation body, inlined (no FBA path): inputs rdi=handle,
/// rsi=n, rdx=elem_size; output rax=payload. Used by `realloc` (which never grows
/// an FBA slice in the corpus). Clobbers rax/rcx/rdx/rsi/r8/r9/r11.
fn emit_alloc_inline(a: &mut Asm) {
    // bytes = n * elem_size -> r8.
    a.mov_rr(Gpr::R8, Gpr::Rsi);
    a.imul_rr(Gpr::R8, Gpr::Rdx);
    // slot addr -> r9.
    reg_slot_addr(a, Gpr::R9, Gpr::Rdi, Gpr::Rcx);
    // Preserve slot/handle/bytes across the clobbering mmap syscall (see emit_alloc).
    a.sub_rsp_imm(32);
    a.mov_store_mem(Gpr::Rsp, 0, Gpr::R9);
    a.mov_store_mem(Gpr::Rsp, 8, Gpr::Rdi);
    a.mov_store_mem(Gpr::Rsp, 16, Gpr::R8);
    // total = round_up(bytes + HDR, PAGE) -> rsi.
    a.mov_rr(Gpr::Rsi, Gpr::R8);
    a.add_ri(Gpr::Rsi, HDR as i32);
    a.add_ri(Gpr::Rsi, (PAGE - 1) as i32);
    a.and_ri(Gpr::Rsi, !(PAGE as i32 - 1));
    emit_mmap(a); // region -> rax
    a.mov_load_mem(Gpr::R9, Gpr::Rsp, 0);
    a.mov_load_mem(Gpr::Rdi, Gpr::Rsp, 8);
    a.mov_load_mem(Gpr::R8, Gpr::Rsp, 16);
    a.add_rsp_imm(32);
    a.test_rr(Gpr::Rax, Gpr::Rax);
    let ok = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ns, ok);
    emit_oom_trap(a);
    a.bind_local(ok);
    // Fill header.
    a.mov_ri(Gpr::Rcx, BLOCK_MAGIC);
    a.mov_store_mem(Gpr::Rax, H_MAGIC, Gpr::Rcx);
    a.mov_store_mem(Gpr::Rax, H_TOTAL_LEN, Gpr::Rsi);
    a.mov_store_mem(Gpr::Rax, H_PAYLOAD_LEN, Gpr::R8);
    a.mov_store32_mem(Gpr::Rax, H_OWNER, Gpr::Rdi);
    a.mov_ri(Gpr::Rcx, 1);
    a.mov_store32_mem(Gpr::Rax, H_LIVE, Gpr::Rcx);
    emit_track(a, Gpr::R9, Gpr::Rax);
    a.add_ri(Gpr::Rax, HDR as i32);
}

/// The free body, inlined for `realloc` (GPA-validated or plain). Inputs
/// rdi=handle, rsi=ptr, rdx=checks_off. Used only on the realloc old-block path,
/// where the block is a real mmap block. For a TRACKED (GPA) allocator the old
/// slice is invalidated like the VM's `heap.realloc` — marked dead and unlinked
/// from the live list, then mprotected for UAF — but the region stays MAPPED for
/// the single `deinit` walk to reclaim (never eagerly munmap-ed, which would
/// dangle a `live_head` node). For the default / checks-off path (no live list)
/// the block is munmap-ed in place.
fn emit_free_inline(a: &mut Asm) {
    // slot addr -> r11; r10 is the old hdr, reusable.
    reg_slot_addr(a, Gpr::R11, Gpr::Rdi, Gpr::Rcx);
    a.mov_load32_mem(Gpr::Rcx, Gpr::R11, RS_KIND);
    let plain = a.new_local_label();
    let done = a.new_local_label();
    // Arena: the old realloc block is tracked on `live_head`, so it must be
    // unlinked (mirroring the VM's `retrack_realloc`, which retains-removes the
    // old cell) and then left MAPPED — never munmap-ed here — so `arena.deinit`'s
    // single walk does not dereference a freed node. (An arena keeps no leak /
    // double-free state, so there is no live flag / count to touch.)
    a.cmp_ri(Gpr::Rcx, KIND_ARENA as i32);
    let not_arena = a.new_local_label();
    a.jcc_local(crate::encode::Cc::Ne, not_arena);
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));
    emit_unlink_block(a, Gpr::R11, Gpr::R10);
    a.jmp_local(done);
    a.bind_local(not_arena);
    // GPA with checks on?
    a.cmp_ri(Gpr::Rcx, KIND_GPA as i32);
    a.jcc_local(crate::encode::Cc::Ne, plain);
    a.test_rr(Gpr::Rdx, Gpr::Rdx);
    a.jcc_local(crate::encode::Cc::Ne, plain);
    // GPA checked path: mark the old block dead, decrement the leak counter, and
    // UNLINK it from the live list — but DO NOT munmap it here. The VM's
    // `heap.realloc` frees the old *cell* (so a stale slice trips use-after-free)
    // yet never returns its backing to the OS until process exit; the native
    // mirror keeps the region MAPPED (header live for double-free probing,
    // payload mprotected for UAF) so the single `deinit` reclamation walk stays
    // consistent. Eagerly munmap-ing here is exactly the v0.16 teardown SIGSEGV:
    // it left the freed block threaded on `live_head` pointing at unmapped memory,
    // which the deinit walk then dereferenced. hdr = ptr - HDR -> r10.
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));
    a.mov_ri(Gpr::Rax, 0);
    a.mov_store32_mem(Gpr::R10, H_LIVE, Gpr::Rax);
    a.mov_load32_mem(Gpr::Rax, Gpr::R11, RS_LIVE_COUNT);
    a.add_ri(Gpr::Rax, -1);
    a.mov_store32_mem(Gpr::R11, RS_LIVE_COUNT, Gpr::Rax);
    // Unlink from live_head (clobbers rax/rcx/rdx; preserves r10/r11).
    emit_unlink_block(a, Gpr::R11, Gpr::R10);
    // mprotect the *entire* payload PROT_NONE so a UAF on the stale old slice
    // faults at any offset, matching `emit_gpa_free`. The payload is page-isolated
    // (`HDR == PAGE`); the header page stays readable. A zero-byte payload has no
    // payload page. addr = hdr + HDR, len = total_len - HDR.
    a.mov_load_mem(Gpr::Rdx, Gpr::R10, H_TOTAL_LEN);
    a.cmp_ri(Gpr::Rdx, (HDR + PAGE) as i32);
    a.jcc_local(crate::encode::Cc::B, done);
    a.mov_rr(Gpr::Rdi, Gpr::R10);
    a.add_ri(Gpr::Rdi, HDR as i32);
    a.mov_rr(Gpr::Rsi, Gpr::Rdx);
    a.add_ri(Gpr::Rsi, -(HDR as i32));
    a.mov_ri(Gpr::Rdx, PROT_NONE);
    a.mov_ri(Gpr::Rax, SYS_MPROTECT);
    a.syscall();
    a.jmp_local(done);
    a.bind_local(plain);
    // Default / checks-off: plain munmap of the real block.
    a.mov_rr(Gpr::R10, Gpr::Rsi);
    a.add_ri(Gpr::R10, -(HDR as i32));
    a.mov_load_mem(Gpr::Rax, Gpr::R10, H_MAGIC);
    a.mov_ri(Gpr::Rcx, BLOCK_MAGIC);
    a.cmp_rr(Gpr::Rax, Gpr::Rcx);
    a.jcc_local(crate::encode::Cc::Ne, done);
    a.mov_load_mem(Gpr::Rsi, Gpr::R10, H_TOTAL_LEN);
    a.mov_rr(Gpr::Rdi, Gpr::R10);
    a.mov_ri(Gpr::Rax, SYS_MUNMAP);
    a.syscall();
    a.bind_local(done);
}

// ---------------------------------------------------------------------
//  __k2_alloc_id(kind=rdi, fba_buf=rsi, fba_cap=rdx) -> handle in rax
// ---------------------------------------------------------------------

fn emit_alloc_id(a: &mut Asm) {
    // handle = reg_next; reg_next += 1.
    a.mov_ri_state(Gpr::R8, ST_REG_NEXT);
    a.mov_load_mem(Gpr::Rax, Gpr::R8, 0); // rax = reg_next
                                          // Bound-check the new handle against the fixed REG_MAX registry: the registry
                                          // lives in a single page-rounded writable PT_LOAD, so minting slot REG_MAX (or
                                          // beyond) would scribble past the mapping — a silent corruption today, a
                                          // SIGSEGV a few dozen handles later. Convert that into a clean, deterministic
                                          // refusal (the VM grows its `allocators` Vec unboundedly, so this is a
                                          // documented native narrowing rather than a divergence on any in-budget
                                          // program). `reg_next >= REG_MAX` traps.
    a.cmp_ri(Gpr::Rax, REG_MAX as i32);
    let in_bounds = a.new_local_label();
    a.jcc_local(crate::encode::Cc::B, in_bounds);
    emit_trap(a, b"panic: too many allocators\n");
    a.bind_local(in_bounds);
    a.mov_rr(Gpr::Rcx, Gpr::Rax);
    a.add_ri(Gpr::Rcx, 1);
    a.mov_store_mem(Gpr::R8, 0, Gpr::Rcx);

    // slot addr -> r9 (from rax = handle).
    reg_slot_addr(a, Gpr::R9, Gpr::Rax, Gpr::Rcx);
    // slot.kind = rdi.
    a.mov_store32_mem(Gpr::R9, RS_KIND, Gpr::Rdi);
    // slot.live_count = 0, live_head = 0, fba_off = 0.
    a.mov_ri(Gpr::Rcx, 0);
    a.mov_store32_mem(Gpr::R9, RS_LIVE_COUNT, Gpr::Rcx);
    a.mov_store_mem(Gpr::R9, RS_LIVE_HEAD, Gpr::Rcx);
    a.mov_store_mem(Gpr::R9, RS_FBA_OFF, Gpr::Rcx);
    // slot.fba_buf = rsi, slot.fba_cap = rdx.
    a.mov_store_mem(Gpr::R9, RS_FBA_BUF, Gpr::Rsi);
    a.mov_store_mem(Gpr::R9, RS_FBA_CAP, Gpr::Rdx);
    // return handle (rax already holds it).
    a.ret();
}

// ---------------------------------------------------------------------
//  __k2_gpa_deinit(handle=rdi, checks_off=rsi) -> bool in rax
// ---------------------------------------------------------------------

fn emit_gpa_deinit(a: &mut Asm) {
    // checks_off -> return 0.
    a.test_rr(Gpr::Rsi, Gpr::Rsi);
    let on = a.new_local_label();
    a.jcc_local(crate::encode::Cc::E, on);
    a.mov_ri(Gpr::Rax, 0);
    a.ret();
    a.bind_local(on);

    // slot addr -> r9.
    reg_slot_addr(a, Gpr::R9, Gpr::Rdi, Gpr::Rcx);
    // leaked = (live_count != 0).
    a.mov_load32_mem(Gpr::Rax, Gpr::R9, RS_LIVE_COUNT);
    a.test_rr(Gpr::Rax, Gpr::Rax);
    a.setcc_al(crate::encode::Cc::Ne);
    a.movzx_al(Gpr::Rax);
    // Stash leaked in r8 across the reclamation walk.
    a.mov_rr(Gpr::R8, Gpr::Rax);

    // Reclaim: walk live_head, munmap every still-mapped block, clear the tracker.
    emit_free_list_walk(a, Gpr::R9);
    // Clear tracker: live_head = 0, live_count = 0.
    a.mov_ri(Gpr::Rcx, 0);
    a.mov_store_mem(Gpr::R9, RS_LIVE_HEAD, Gpr::Rcx);
    a.mov_store32_mem(Gpr::R9, RS_LIVE_COUNT, Gpr::Rcx);
    // return leaked.
    a.mov_rr(Gpr::Rax, Gpr::R8);
    a.ret();
}

/// Walks the live-block list of the slot in `slot_reg`, `munmap`-ing every block.
/// The `next` link is held on the stack across the `munmap` syscall (which clobbers
/// rcx / r11), so the walk cannot lose the chain. Clobbers rax/rcx/rdi/rsi/r10/r11
/// and reads the slot's `live_head`.
fn emit_free_list_walk(a: &mut Asm, slot_reg: Gpr) {
    // cur (r10) = live_head.
    a.mov_load_mem(Gpr::R10, slot_reg, RS_LIVE_HEAD);
    let loop_top = a.new_local_label();
    let loop_end = a.new_local_label();
    a.bind_local(loop_top);
    a.test_rr(Gpr::R10, Gpr::R10);
    a.jcc_local(crate::encode::Cc::E, loop_end);
    // Save cur and next across the syscall (syscall clobbers rcx/r11; we also keep
    // r10 live). Reserve a 16-byte aligned frame holding [next, cur].
    a.mov_load_mem(Gpr::R11, Gpr::R10, H_NEXT); // next
    a.sub_rsp_imm(16);
    a.mov_store_mem(Gpr::Rsp, 0, Gpr::R11); // next
                                            // munmap(cur, cur.total_len).
    a.mov_load_mem(Gpr::Rsi, Gpr::R10, H_TOTAL_LEN);
    a.mov_rr(Gpr::Rdi, Gpr::R10);
    a.mov_ri(Gpr::Rax, SYS_MUNMAP);
    a.syscall();
    // cur = next (reloaded from the stack, since r11 was clobbered).
    a.mov_load_mem(Gpr::R10, Gpr::Rsp, 0);
    a.add_rsp_imm(16);
    a.jmp_local(loop_top);
    a.bind_local(loop_end);
}

// ---------------------------------------------------------------------
//  __k2_arena_deinit(handle=rdi)
// ---------------------------------------------------------------------

fn emit_arena_deinit(a: &mut Asm) {
    // slot addr -> r9.
    reg_slot_addr(a, Gpr::R9, Gpr::Rdi, Gpr::Rcx);
    // Walk + munmap every block the arena handed out (bulk free).
    emit_free_list_walk(a, Gpr::R9);
    a.mov_ri(Gpr::Rcx, 0);
    a.mov_store_mem(Gpr::R9, RS_LIVE_HEAD, Gpr::Rcx);
    a.mov_store32_mem(Gpr::R9, RS_LIVE_COUNT, Gpr::Rcx);
    a.ret();
}

// ---------------------------------------------------------------------
//  Syscall + trap helpers
// ---------------------------------------------------------------------

/// `mmap(addr=0, len=rsi, prot=RW, flags=PRIVATE|ANON, fd=-1, off=0)`. Result
/// (the region base, or `-errno` in [-4095,-1]) -> rax. Clobbers the syscall arg
/// registers; `rsi` (len) is the caller's input.
fn emit_mmap(a: &mut Asm) {
    a.mov_ri(Gpr::Rdi, 0); // addr
                           // rsi = len (input).
    a.mov_ri(Gpr::Rdx, PROT_RW);
    a.mov_ri(Gpr::R10, MAP_PRIVATE_ANON);
    a.mov_ri(Gpr::R8, -1); // fd
    a.mov_ri(Gpr::R9, 0); // offset
    a.mov_ri(Gpr::Rax, SYS_MMAP);
    a.syscall();
}

/// A clean out-of-memory trap: writes `panic: out of memory\n` to stderr and
/// `exit(134)`. Reached only on a true `mmap` failure (the corpus never hits it on
/// real hardware), matching the VM's clean OOM panic for an unbounded allocator.
fn emit_oom_trap(a: &mut Asm) {
    emit_trap(a, b"panic: out of memory\n");
}

/// Emits a clean trap: `write(2, msg, len); exit(134)`. The message bytes are
/// emitted inline in `.text` and addressed with a RIP-relative `lea`-free trick:
/// we place the bytes after an unconditional jump over them and `lea` their
/// address via a `call`/`pop` thunk. To keep the encoder minimal we instead embed
/// the string immediately after a `jmp` and compute its absolute address from a
/// `call`-relative return address.
fn emit_trap(a: &mut Asm, msg: &[u8]) {
    // Strategy: `call after; <bytes>; after: pop rsi` makes rsi point just past
    // the call (i.e. at the message bytes). We use a local label for `after` so the
    // `call` rel32 is patched by `Asm::finish`.
    let after = a.new_local_label();
    a.call_label(after);
    // The message bytes live here, right after the call instruction.
    a.emit_bytes(msg);
    a.bind_local(after);
    // rsi = return address pushed by `call` = address of the message bytes.
    a.pop(Gpr::Rsi);
    a.mov_ri(Gpr::Rdi, 2); // stderr
    a.mov_ri(Gpr::Rdx, msg.len() as i64);
    a.mov_ri(Gpr::Rax, 1); // write
    a.syscall();
    a.mov_ri(Gpr::Rdi, TRAP_EXIT);
    a.mov_ri(Gpr::Rax, SYS_EXIT);
    a.syscall();
}
