//! Round-robin preemptive scheduler.
//!
//! Single ready queue, single running process, single idle process, single
//! CPU. SMP is explicitly out of scope for v1 — see `README.md` for the
//! migration path.
//!
//! Concurrency model: the entire scheduler critical section runs with
//! interrupts disabled on the current CPU (`IrqGuard`). Because this is
//! single-CPU, that is sufficient mutual exclusion. SMP will need a real
//! spinlock layered on top.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::scheduler::context::context_switch;
use crate::scheduler::process::{
    EntryFn, OwnerId, Pid, Process, ProcessState, PID_IDLE,
};

/// Default per-process time slice, in timer ticks. Picked to match a 10 ms
/// quantum at the kernel's 1 kHz timer rate; tune from userspace later.
pub const DEFAULT_TIME_SLICE: u32 = 10;

/// Errors the scheduler may report at its API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedError {
    /// `init()` was not called, or was called more than once.
    NotInitialized,
    AlreadyInitialized,
    /// No such PID is currently blocked.
    NoSuchBlockedPid,
}

/// The scheduler's mutable state. Wrapped inside `GlobalScheduler` for the
/// kernel-global instance; exposed directly only for testing.
pub struct Scheduler {
    initialized: bool,

    /// Tasks waiting for CPU, in FIFO order.
    ready: VecDeque<Box<Process>>,

    /// The task currently on-CPU. `None` only during early bootstrap before
    /// the first `set_current` call.
    current: Option<Box<Process>>,

    /// Idle task. Always present after `init()`; never enqueued in `ready`.
    /// Run when `ready` is empty and `current` is not Running.
    idle: Option<Box<Process>>,

    /// Tasks parked on a kernel waitqueue. Kept here only so they don't get
    /// dropped — the waitqueue owns the "wake them up" decision.
    blocked: Vec<Box<Process>>,

    /// Processes that called `exit_current` and need their stacks freed.
    /// Drained at the *top* of `schedule()`, which is guaranteed to be
    /// running on some other process's stack by that point.
    defunct: Vec<Box<Process>>,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            initialized: false,
            ready: VecDeque::new(),
            current: None,
            idle: None,
            blocked: Vec::new(),
            defunct: Vec::new(),
        }
    }

    /// Install the idle process and a bootstrap PCB representing the
    /// currently-running kernel thread. Must be called exactly once, before
    /// any other scheduler API.
    pub fn init(
        &mut self,
        bootstrap_owner: OwnerId,
        idle_stack: Box<[u8]>,
        idle_entry: EntryFn,
    ) -> Result<(), SchedError> {
        if self.initialized {
            return Err(SchedError::AlreadyInitialized);
        }
        let idle = Box::new(Process::new_with_pid(
            PID_IDLE,
            String::from("idle"),
            bootstrap_owner,
            idle_entry,
            idle_stack,
            DEFAULT_TIME_SLICE,
        ));
        self.idle = Some(idle);
        self.current = Some(Box::new(Process::bootstrap(bootstrap_owner)));
        self.initialized = true;
        Ok(())
    }

    /// Queue a freshly-built process for execution. Sets state to Ready and
    /// places it at the back of the queue.
    pub fn spawn(&mut self, mut proc: Box<Process>) -> Pid {
        proc.state = ProcessState::Ready;
        proc.time_slice_remaining = proc.default_time_slice;
        let pid = proc.pid;
        self.ready.push_back(proc);
        pid
    }

    /// Timer-tick hook. Decrements the running task's slice; on exhaustion,
    /// triggers a round-robin preemption.
    ///
    /// Must be called from the timer IRQ handler (which is itself responsible
    /// for saving the volatile register set before invoking us).
    pub fn tick(&mut self) {
        let needs_preempt = match self.current.as_mut() {
            Some(curr) if curr.state == ProcessState::Running => {
                if curr.time_slice_remaining > 0 {
                    curr.time_slice_remaining -= 1;
                }
                curr.time_slice_remaining == 0
            }
            _ => false,
        };
        if needs_preempt {
            self.schedule();
        }
    }

    /// Voluntary yield. Re-queues the current task and picks the next one.
    pub fn yield_now(&mut self) {
        self.schedule();
    }

    /// Mark the running task as Zombie and switch away. Never returns.
    ///
    /// The Process and its stack are *not* freed here — they're parked in
    /// `defunct` and reaped at the top of the next `schedule()` call, at
    /// which point we are guaranteed to be running on someone else's stack.
    pub fn exit_current(&mut self) -> ! {
        if let Some(curr) = self.current.as_mut() {
            curr.state = ProcessState::Zombie;
            curr.time_slice_remaining = 0;
        }
        self.schedule();
        // schedule() set current to someone else and context-switched away;
        // we can only "return" here if we were re-scheduled, but a Zombie
        // never re-enters the ready queue, so this is unreachable.
        unreachable!("exited process was re-scheduled");
    }

    /// Park the running task as Blocked. The caller (a waitqueue, mutex, etc.)
    /// is responsible for eventually calling `unblock(pid)` to make it Ready
    /// again.
    pub fn block_current(&mut self) {
        if let Some(curr) = self.current.as_mut() {
            curr.state = ProcessState::Blocked;
        }
        self.schedule();
    }

    /// Move a previously-blocked task back into the ready queue.
    pub fn unblock(&mut self, pid: Pid) -> Result<(), SchedError> {
        let idx = self
            .blocked
            .iter()
            .position(|p| p.pid == pid)
            .ok_or(SchedError::NoSuchBlockedPid)?;
        let mut proc = self.blocked.swap_remove(idx);
        proc.state = ProcessState::Ready;
        proc.time_slice_remaining = proc.default_time_slice;
        self.ready.push_back(proc);
        Ok(())
    }

    /// Read-only inspection of the current PID — handy for logging/audit.
    pub fn current_pid(&self) -> Option<Pid> {
        self.current.as_ref().map(|p| p.pid)
    }

    /// Core of the scheduler. Picks the next task and context-switches.
    ///
    /// Preconditions: called with interrupts disabled (typically via the
    /// `IrqGuard` held by `GlobalScheduler::with`).
    fn schedule(&mut self) {
        // Reap anything left over by a previous `exit_current`. Safe to drop
        // here because we are, by construction, NOT on any defunct process's
        // stack at the entry to schedule() — the only way to enter schedule()
        // is from a currently-live task.
        self.defunct.clear();

        // Decide whom to run next. The current task is re-queued only if it
        // is still Running (i.e., this is preemption or voluntary yield, not
        // exit/block).
        let next = match self.ready.pop_front() {
            Some(n) => n,
            None => {
                // Nothing in the queue. If current is still Running, just keep
                // running it — no point in a self-switch.
                if self
                    .current
                    .as_ref()
                    .map_or(false, |c| c.state == ProcessState::Running)
                {
                    return;
                }
                // Otherwise fall back to the idle task.
                match self.idle.take() {
                    Some(i) => i,
                    None => return, // not initialized; nothing we can do
                }
            }
        };
        let next_rsp = next.kernel_rsp;

        // Capture a raw pointer to the outgoing task's RSP save slot BEFORE
        // we move the Box, so we don't hold a borrow across the move.
        // If there is no current task (very early boot), point at a local
        // throwaway — the value written by context_switch is never read.
        let mut dummy_save: usize = 0;
        let prev_rsp_ptr: *mut usize = match self.current.as_mut() {
            Some(curr) => &mut curr.kernel_rsp as *mut usize,
            None => &mut dummy_save as *mut usize,
        };

        // Reclassify the outgoing task and stash it where it belongs.
        // The Box is moved, but the Process data lives on the heap — the
        // raw `prev_rsp_ptr` above remains valid through the move.
        if let Some(mut curr) = self.current.take() {
            match curr.state {
                ProcessState::Running => {
                    curr.state = ProcessState::Ready;
                    curr.time_slice_remaining = curr.default_time_slice;
                    // The idle task gets its dedicated slot back; everyone
                    // else lines up in ready.
                    if curr.pid == PID_IDLE {
                        self.idle = Some(curr);
                    } else {
                        self.ready.push_back(curr);
                    }
                }
                ProcessState::Ready => {
                    // Shouldn't happen for `current` under normal flow, but
                    // be permissive and just requeue.
                    self.ready.push_back(curr);
                }
                ProcessState::Blocked => {
                    self.blocked.push(curr);
                }
                ProcessState::Zombie => {
                    self.defunct.push(curr);
                }
            }
        }

        // Install the new current and switch.
        self.current = Some(next);
        // The borrow on `self.current` ends before the unsafe block; safe to
        // re-borrow mutably.
        if let Some(c) = self.current.as_mut() {
            c.state = ProcessState::Running;
            c.time_slice_remaining = c.default_time_slice;
        }

        // SAFETY: see `context::context_switch` for the full contract.
        // Recap:
        //   * `prev_rsp_ptr` points either into a Process whose Box is now
        //     parked in self.ready / self.blocked / self.defunct (heap data
        //     lives at a stable address through the Box move above) or into
        //     `dummy_save` on this stack frame (safe to write; never read).
        //   * `next_rsp` was either produced by `init_kernel_stack` for a
        //     first-run process, or was saved by a previous `context_switch`
        //     into a Process still alive in self.current (we just installed
        //     it).
        //   * Interrupts are disabled by our caller's `IrqGuard`; the new
        //     task's first run re-enables them in `process_entry_trampoline`,
        //     while a resumed task re-enables them when its own original
        //     `IrqGuard` drops further up its stack.
        unsafe {
            context_switch(prev_rsp_ptr, next_rsp);
        }

        // Returning here means *we* were just resumed onto this stack from
        // some other task's call to `schedule`. Caller's `IrqGuard` will be
        // dropped on its way out, restoring our original interrupt state.
    }
}

// -----------------------------------------------------------------------------
// IRQ-safe critical section
// -----------------------------------------------------------------------------

/// RAII guard that disables maskable interrupts on the current CPU for the
/// duration of the scope. Saves and restores RFLAGS so nested guards behave
/// correctly: if interrupts were already disabled at construction, drop
/// leaves them disabled.
pub struct IrqGuard {
    saved_flags: u64,
}

impl IrqGuard {
    pub fn new() -> Self {
        let flags: u64;
        // SAFETY: `pushfq` reads RFLAGS into the stack; `pop` moves it to a
        // GPR; `cli` masks maskable interrupts. None of this touches memory
        // outside the inline-asm-managed scratch slot. Single-instruction
        // semantics; cannot race against anything before it completes.
        unsafe {
            core::arch::asm!(
                "pushfq",
                "pop {f}",
                "cli",
                f = out(reg) flags,
                options(preserves_flags)
            );
        }
        Self { saved_flags: flags }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        // SAFETY: restore exactly the RFLAGS we captured in `new`. If IF was
        // set then, this re-enables interrupts; if it was clear, they remain
        // disabled. We never modify any other process's CPU state — this
        // guard is per-thread.
        unsafe {
            core::arch::asm!(
                "push {f}",
                "popfq",
                f = in(reg) self.saved_flags,
                options()
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Global instance
// -----------------------------------------------------------------------------

/// Single-CPU singleton wrapping `Scheduler`. Access is serialized by the
/// `IrqGuard` inside `with`.
pub struct GlobalScheduler {
    inner: UnsafeCell<Scheduler>,
}

// SAFETY: All `&mut Scheduler` access goes through `with`, which holds an
// `IrqGuard` for its entire body. On a single CPU, disabling interrupts is
// sufficient mutual exclusion: no other code path can be "running" anywhere
// on this CPU until we re-enable interrupts. SMP support will need to layer
// a real spinlock on top of this guard.
unsafe impl Sync for GlobalScheduler {}

impl GlobalScheduler {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Scheduler::new()),
        }
    }

    /// Run `f` with exclusive access to the scheduler. Disables interrupts
    /// for the duration.
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Scheduler) -> R,
    {
        let _guard = IrqGuard::new();
        // SAFETY: IrqGuard disables interrupts on this CPU, and this is the
        // only code path that hands out `&mut Scheduler`. Therefore no other
        // borrow of `*self.inner.get()` can exist on this CPU for the
        // duration of the closure.
        unsafe { f(&mut *self.inner.get()) }
    }
}

/// The one scheduler the kernel uses.
pub static SCHEDULER: GlobalScheduler = GlobalScheduler::new();
