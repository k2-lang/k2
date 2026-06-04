//! The deterministic cooperative fiber scheduler — the v0.11 concurrency core.
//!
//! # Why a fiber scheduler (and what is deferred to native codegen)
//!
//! The VM is single-threaded, so "parallelism" is realized here as *interleaved
//! green fibers* driven by an explicit, capability-passed event loop. Each fiber
//! is a green thread with **its own call-frame stack** (a `Vec<Frame>` moved out
//! of the VM); a deterministic scheduler runs ready fibers to their next yield
//! point (a `spawn`, a channel `send`/`recv`, a `mutex` acquire, an `await`, or
//! an explicit `yield`). Output is reproducible because the scheduler is a
//! deterministic state machine: a FIFO ready queue plus FIFO waiter lists, with
//! a fixed tie-break order, so the interleaving is identical run-to-run.
//!
//! This is the **VM realization** of the spec's colorless async/`Executor` API.
//! It is *not* linked into a native k2 binary:
//!
//! - **Native codegen (post-0.13)** lowers async to the stackless `Frame`
//!   state machine of [spec §7](../../../../docs/spec/09-concurrency.md) and maps
//!   `Executor` to OS threads. Memory `Ordering` becomes real fences there; here
//!   it is accepted and ignored (a single thread cannot interleave mid-op, so an
//!   "atomic" read-modify-write is trivially correct).
//! - **Both realizations honor the identical, capability-passed, keyword-free
//!   surface**: an `Executor`/`Loop` is a library value built from `sys.heap`,
//!   passed explicitly, never a global; `spawn`/`await`/`join`/`Mutex`/`atomic`/
//!   `WaitGroup` are ordinary method calls; `await` is a method name in
//!   identifier position, not a keyword.
//!
//! # Never hangs, never panics the host
//!
//! Every block reason carries an explicit waker, and the ready queue is the only
//! source of progress, so an empty ready queue with live (blocked) fibers is
//! *provably* a deadlock — detected immediately and reported as a clean program
//! panic (never a hang, never a Rust panic). The VM's existing step-budget /
//! wall-clock guards remain as a backstop for a livelock (fibers that endlessly
//! yield doing nothing).

use std::collections::VecDeque;

use crate::isa::Reg;
use crate::value::Value;

/// A green-thread identifier: an index into [`Scheduler::fibers`].
pub type FiberId = u32;

/// A channel identifier: an index into [`Scheduler::channels`].
pub type ChanId = u32;

/// A mutex identifier: an index into [`Scheduler::mutexes`].
pub type MutexId = u32;

/// A wait-group identifier: an index into [`Scheduler::waitgroups`].
pub type WgId = u32;

/// An atomic-cell identifier: an index into [`Scheduler::atomics`].
pub type AtomicId = u32;

/// One call frame: the callee, its register file, and its program counter. A
/// fiber owns a stack of these (lifted out of the VM so each green thread has its
/// own independent call stack).
pub struct Frame {
    /// The compiled function this frame is executing.
    pub fnid: k2_mir::FnId,
    /// The register file (registers 1:1 with MIR locals, plus scratch).
    pub regs: Vec<Value>,
    /// The instruction pointer within the function's code.
    pub pc: usize,
    /// The caller's destination register for this call's result.
    pub ret_reg: Reg,
}

/// The lifecycle state of a fiber.
pub enum FiberState {
    /// On the ready queue (or about to be), runnable now.
    Ready,
    /// Currently executing (the scheduler's `current`).
    Running,
    /// Parked on a wait reason; not in the ready queue until a waker re-readies
    /// it. The reason carries enough to wake it event-driven (no polling).
    Blocked(BlockReason),
    /// The fiber's root frame returned; its `result` is set.
    Done,
}

/// Why a fiber is parked. Each variant names the resource whose change wakes it,
/// so wake-ups are O(1) and event-driven rather than polled.
pub enum BlockReason {
    /// Awaiting another fiber's completion (`join`/`await`).
    Join(FiberId),
    /// A `recv` on an empty, not-yet-closed channel.
    ChanRecv(ChanId),
    /// A `send` on a full bounded channel; carries the value to admit on wake.
    /// The fiber is also queued in the channel's `send_waiters`, so a receiver
    /// finds it there and reads this `value` to enqueue.
    ChanSend { value: Value },
    /// A `lock` on a mutex held by another fiber.
    MutexLock(MutexId),
    /// A `wait` on a wait-group whose counter is still positive.
    WaitGroup(WgId),
}

impl BlockReason {
    /// A human label (with the resource id) for the deadlock diagnostic, so the
    /// message names the specific channel/mutex/task each stuck fiber awaits.
    pub fn label(&self) -> String {
        match self {
            BlockReason::Join(id) => format!("await(task#{id})"),
            BlockReason::ChanRecv(id) => format!("recv(chan#{id})"),
            BlockReason::ChanSend { .. } => "send(full chan)".to_string(),
            BlockReason::MutexLock(id) => format!("lock(mutex#{id})"),
            BlockReason::WaitGroup(id) => format!("wait(wg#{id})"),
        }
    }
}

/// A green thread: its own call-frame stack, completion state, and joiners.
pub struct Fiber {
    /// This fiber's call stack (the per-VM `frames` Vec, moved here).
    pub frames: Vec<Frame>,
    /// Its lifecycle state.
    pub state: FiberState,
    /// The value its root frame returned (set on completion, delivered to
    /// joiners).
    pub result: Option<Value>,
    /// Fibers blocked in `join`/`await` on THIS fiber, woken on completion. FIFO
    /// so the woken order is deterministic.
    pub joiners: Vec<FiberId>,
    /// The register in *this* fiber's top frame that a resume must fill (the dst
    /// of the blocking intrinsic). Set on block, consumed on wake.
    pub resume_reg: Option<Reg>,
}

impl Fiber {
    /// A fresh ready fiber with the given root frame.
    fn new(root: Frame) -> Fiber {
        Fiber {
            frames: vec![root],
            state: FiberState::Ready,
            result: None,
            joiners: Vec::new(),
            resume_reg: None,
        }
    }
}

/// A bounded/unbounded FIFO channel for fiber communication.
pub struct Channel {
    /// `None` is unbounded; `Some(n)` bounds the queue to `n` buffered values.
    pub cap: Option<usize>,
    /// The buffered values, in send order (so `recv` preserves order).
    pub queue: VecDeque<Value>,
    /// Whether the channel has been closed (no more sends will be admitted).
    pub closed: bool,
    /// Senders parked because the bounded queue is full, in FIFO order.
    pub send_waiters: VecDeque<FiberId>,
    /// Receivers parked because the queue is empty, in FIFO order.
    pub recv_waiters: VecDeque<FiberId>,
}

/// A cooperative mutex cell.
pub struct MutexCell {
    /// The fiber currently holding the lock, if any.
    pub held_by: Option<FiberId>,
    /// Fibers waiting to acquire, in FIFO order (hand-off is fair).
    pub waiters: VecDeque<FiberId>,
}

/// A wait-group cell: a counter plus the fibers blocked until it reaches zero.
pub struct WaitGroupCell {
    /// The outstanding count.
    pub count: i64,
    /// Fibers blocked in `wait`, all woken when `count` reaches zero.
    pub waiters: VecDeque<FiberId>,
}

/// The deterministic cooperative scheduler: many fibers, one FIFO ready queue,
/// and the object tables for channels/mutexes/wait-groups/atomics.
pub struct Scheduler {
    /// All fibers, dense, indexed by [`FiberId`].
    pub fibers: Vec<Fiber>,
    /// The FIFO ready queue — the determinism backbone.
    pub ready: VecDeque<FiberId>,
    /// The currently running fiber.
    pub current: FiberId,
    /// Registered channels, indexed by [`ChanId`].
    pub channels: Vec<Channel>,
    /// Registered mutexes, indexed by [`MutexId`].
    pub mutexes: Vec<MutexCell>,
    /// Registered wait-groups, indexed by [`WgId`].
    pub waitgroups: Vec<WaitGroupCell>,
    /// Atomic backing cells, indexed by [`AtomicId`]. A single `i128` per cell is
    /// enough for every integer/bool `Atomic(T)` the std exposes; under the
    /// cooperative model an RMW cannot be interleaved, so it is trivially atomic.
    pub atomics: Vec<i128>,
}

impl Default for Scheduler {
    fn default() -> Scheduler {
        Scheduler::new()
    }
}

impl Scheduler {
    /// A fresh scheduler with no fibers and empty object tables.
    pub fn new() -> Scheduler {
        Scheduler {
            fibers: Vec::new(),
            ready: VecDeque::new(),
            current: 0,
            channels: Vec::new(),
            mutexes: Vec::new(),
            waitgroups: Vec::new(),
            atomics: Vec::new(),
        }
    }

    /// Registers a fiber with the given root frame, enqueues it Ready, and returns
    /// its id. The caller has already built the root [`Frame`] (params seeded).
    pub fn spawn_fiber(&mut self, root: Frame) -> FiberId {
        let id = self.fibers.len() as FiberId;
        self.fibers.push(Fiber::new(root));
        self.ready.push_back(id);
        id
    }

    /// The current fiber's call-frame stack (immutable).
    pub fn cur_frames(&self) -> &Vec<Frame> {
        &self.fibers[self.current as usize].frames
    }

    /// The current fiber's call-frame stack (mutable).
    pub fn cur_frames_mut(&mut self) -> &mut Vec<Frame> {
        &mut self.fibers[self.current as usize].frames
    }

    /// `true` once every fiber has completed.
    pub fn all_done(&self) -> bool {
        self.fibers
            .iter()
            .all(|f| matches!(f.state, FiberState::Done))
    }

    /// The number of fibers currently blocked (for the deadlock diagnostic).
    pub fn blocked_count(&self) -> usize {
        self.fibers
            .iter()
            .filter(|f| matches!(f.state, FiberState::Blocked(_)))
            .count()
    }

    /// `true` if some *other* fiber is runnable (ready). Used by `@schedRun` to
    /// decide whether the running fiber should keep yielding to drain pending
    /// work, or return because the ready set is otherwise empty.
    pub fn other_runnable(&self) -> bool {
        self.ready.iter().any(|&id| id != self.current)
    }

    /// Parks the current fiber on `reason`, recording `dst` as the register a
    /// later wake should fill. The fiber is NOT enqueued; only its waker re-readies
    /// it (or, for `Yield`, the event loop re-enqueues it immediately).
    pub fn block_current(&mut self, reason: BlockReason, dst: Reg) {
        let id = self.current;
        let f = &mut self.fibers[id as usize];
        f.resume_reg = Some(dst);
        f.state = FiberState::Blocked(reason);
    }

    /// Re-readies fiber `id`, optionally delivering `value` into its parked
    /// `resume_reg` (the dst of the blocking intrinsic). A `Done` fiber is ignored.
    pub fn wake(&mut self, id: FiberId, value: Option<Value>) {
        let f = &mut self.fibers[id as usize];
        if matches!(f.state, FiberState::Done) {
            return;
        }
        if let (Some(reg), Some(val)) = (f.resume_reg.take(), value) {
            if let Some(frame) = f.frames.last_mut() {
                frame.regs[reg as usize] = val;
            }
        }
        f.state = FiberState::Ready;
        self.ready.push_back(id);
    }

    // ---- channels -----------------------------------------------------

    /// Registers a channel. `cap < 0` is unbounded; `cap >= 0` is bounded to that
    /// many buffered values.
    pub fn make_channel(&mut self, cap: i64) -> ChanId {
        let id = self.channels.len() as ChanId;
        self.channels.push(Channel {
            cap: if cap < 0 { None } else { Some(cap as usize) },
            queue: VecDeque::new(),
            closed: false,
            send_waiters: VecDeque::new(),
            recv_waiters: VecDeque::new(),
        });
        id
    }

    /// Marks a channel closed and wakes every parked receiver (so they observe
    /// the drained/closed state) and every parked sender (whose send now fails).
    pub fn close_channel(&mut self, chan: ChanId) {
        let Some(ch) = self.channels.get_mut(chan as usize) else {
            return;
        };
        ch.closed = true;
        let recvs: Vec<FiberId> = ch.recv_waiters.drain(..).collect();
        let sends: Vec<FiberId> = ch.send_waiters.drain(..).collect();
        // Receivers wake with the closed sentinel (null optional).
        for r in recvs {
            self.wake(r, Some(Value::Optional(None)));
        }
        // Senders wake with `false` (their send failed because the channel closed).
        for s in sends {
            self.wake(s, Some(Value::Bool(false)));
        }
    }

    /// The number of buffered values in a channel.
    pub fn channel_len(&self, chan: ChanId) -> usize {
        self.channels
            .get(chan as usize)
            .map(|c| c.queue.len())
            .unwrap_or(0)
    }

    // ---- mutexes ------------------------------------------------------

    /// Registers a fresh, unheld mutex.
    pub fn make_mutex(&mut self) -> MutexId {
        let id = self.mutexes.len() as MutexId;
        self.mutexes.push(MutexCell {
            held_by: None,
            waiters: VecDeque::new(),
        });
        id
    }

    // ---- wait-groups --------------------------------------------------

    /// Registers a fresh wait-group with a zero counter.
    pub fn make_waitgroup(&mut self) -> WgId {
        let id = self.waitgroups.len() as WgId;
        self.waitgroups.push(WaitGroupCell {
            count: 0,
            waiters: VecDeque::new(),
        });
        id
    }

    // ---- atomics ------------------------------------------------------

    /// Registers a fresh atomic cell initialized to `init`.
    pub fn make_atomic(&mut self, init: i128) -> AtomicId {
        let id = self.atomics.len() as AtomicId;
        self.atomics.push(init);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::IntRepr;

    /// A throwaway fiber with a single empty frame, used to exercise the queue /
    /// waker bookkeeping without the full VM.
    fn dummy_frame() -> Frame {
        Frame {
            fnid: k2_mir::FnId(0),
            regs: vec![Value::Unit; 4],
            pc: 0,
            ret_reg: 0,
        }
    }

    #[test]
    fn ready_queue_is_fifo() {
        let mut s = Scheduler::new();
        let a = s.spawn_fiber(dummy_frame());
        let b = s.spawn_fiber(dummy_frame());
        let c = s.spawn_fiber(dummy_frame());
        assert_eq!(s.ready.pop_front(), Some(a));
        assert_eq!(s.ready.pop_front(), Some(b));
        assert_eq!(s.ready.pop_front(), Some(c));
        assert!(s.ready.is_empty());
    }

    #[test]
    fn wake_delivers_value_into_resume_register() {
        let mut s = Scheduler::new();
        let f = s.spawn_fiber(dummy_frame());
        s.ready.clear();
        s.current = f;
        // Park it, then wake with a value targeting register 2.
        s.block_current(BlockReason::MutexLock(0), 2);
        s.wake(f, Some(Value::int(99, IntRepr::USIZE)));
        assert_eq!(s.ready.pop_front(), Some(f));
        let v = &s.fibers[f as usize].frames.last().unwrap().regs[2];
        assert_eq!(v.as_i128(), Some(99));
    }

    #[test]
    fn all_done_vs_blocked_discrimination() {
        let mut s = Scheduler::new();
        let f = s.spawn_fiber(dummy_frame());
        s.current = f;
        assert!(!s.all_done());
        s.block_current(BlockReason::MutexLock(0), 0);
        // Blocked, not done: the deadlock detector relies on this.
        assert!(!s.all_done());
        assert_eq!(s.blocked_count(), 1);
        s.fibers[f as usize].state = FiberState::Done;
        assert!(s.all_done());
        assert_eq!(s.blocked_count(), 0);
    }

    #[test]
    fn channel_bounded_full_then_drains_in_order() {
        let mut s = Scheduler::new();
        let ch = s.make_channel(2);
        let c = &mut s.channels[ch as usize];
        c.queue.push_back(Value::int(1, IntRepr::USIZE));
        c.queue.push_back(Value::int(2, IntRepr::USIZE));
        assert_eq!(s.channel_len(ch), 2);
        let c = &mut s.channels[ch as usize];
        assert_eq!(c.queue.pop_front().unwrap().as_i128(), Some(1));
        assert_eq!(c.queue.pop_front().unwrap().as_i128(), Some(2));
    }

    #[test]
    fn close_wakes_receivers_with_null() {
        let mut s = Scheduler::new();
        let ch = s.make_channel(0);
        let r = s.spawn_fiber(dummy_frame());
        s.ready.clear();
        s.current = r;
        s.block_current(BlockReason::ChanRecv(ch), 1);
        s.channels[ch as usize].recv_waiters.push_back(r);
        s.close_channel(ch);
        // The receiver is re-readied with a null (closed sentinel) in reg 1.
        assert_eq!(s.ready.pop_front(), Some(r));
        assert!(matches!(
            s.fibers[r as usize].frames.last().unwrap().regs[1],
            Value::Optional(None)
        ));
    }

    #[test]
    fn waitgroup_and_atomic_cells_register_independently() {
        let mut s = Scheduler::new();
        let wg = s.make_waitgroup();
        s.waitgroups[wg as usize].count = 3;
        let a0 = s.make_atomic(7);
        let a1 = s.make_atomic(0);
        assert_ne!(a0, a1);
        assert_eq!(s.atomics[a0 as usize], 7);
        assert_eq!(s.waitgroups[wg as usize].count, 3);
    }
}
