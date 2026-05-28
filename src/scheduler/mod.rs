//! Process scheduler for the Golem kernel (gkern).
//!
//! Round-robin, preemptive, single-CPU. See `README.md` for the design
//! rationale behind every decision in here.
//!
//! # Module layout
//!
//! * [`process`] — the Process Control Block, PID allocation, kernel-stack
//!   layout.
//! * [`scheduler`] — the ready queue, current/idle/blocked/defunct slots,
//!   round-robin policy, IRQ-safe critical section.
//! * [`context`] — x86_64 context switch (assembly + the Rust extern decls
//!   that wrap it).
//!
//! # Dependency assumptions
//!
//! This module is consumed by the kernel crate, which is responsible for:
//!
//! * `extern crate alloc;` at the crate root (we use `Box`, `Vec`, `String`,
//!   `VecDeque`).
//! * Wiring `tick()` into the timer IRQ handler.
//! * Saving the volatile (caller-saved) register set in the timer IRQ entry
//!   stub *before* invoking `tick()` — `context_switch` only touches the
//!   callee-saved set.
//! * Providing kernel stacks (as `Box<[u8]>`) to `spawn` and `init`.

pub mod context;
pub mod process;
pub mod scheduler;

pub use process::{
    alloc_pid, EntryFn, OwnerId, Pid, Process, ProcessState, PID_BOOTSTRAP, PID_IDLE,
};
pub use scheduler::{SchedError, Scheduler, DEFAULT_TIME_SLICE, SCHEDULER};

use alloc::boxed::Box;
use alloc::string::String;

/// Initialize the scheduler.
///
/// Must be called exactly once during kernel bootstrap, before any other
/// scheduler API. Installs the idle task and records the currently-executing
/// kernel thread as the bootstrap PCB so the first context switch off it has
/// somewhere to save state.
///
/// * `bootstrap_owner` — accountability ID for the kernel itself; Sentinel
///   will be queried for this once it is online.
/// * `idle_stack` — kernel stack for the idle process. At least 80 bytes;
///   4 KiB is the recommended floor.
/// * `idle_entry` — entry point for the idle process. Conventionally a
///   `loop { hlt }` provided by the kernel's power-management module.
pub fn init(
    bootstrap_owner: OwnerId,
    idle_stack: Box<[u8]>,
    idle_entry: EntryFn,
) -> Result<(), SchedError> {
    SCHEDULER.with(|s| s.init(bootstrap_owner, idle_stack, idle_entry))
}

/// Spawn a new kernel task.
///
/// Allocates a PID, synthesizes a launch frame on `kernel_stack`, and queues
/// the task at the back of the ready queue. Returns the assigned PID.
pub fn spawn(
    name: String,
    owner: OwnerId,
    entry: EntryFn,
    kernel_stack: Box<[u8]>,
) -> Pid {
    let proc = Box::new(Process::new(
        name,
        owner,
        entry,
        kernel_stack,
        DEFAULT_TIME_SLICE,
    ));
    SCHEDULER.with(|s| s.spawn(proc))
}

/// Voluntarily yield the CPU. The current task is requeued at the back of
/// the ready queue and another runnable task (or idle) is scheduled.
pub fn yield_now() {
    SCHEDULER.with(|s| s.yield_now());
}

/// Terminate the calling task. Does not return.
///
/// The task transitions to Zombie, is parked in the scheduler's `defunct`
/// list, and is dropped (along with its kernel stack) on the next call to
/// `schedule()` from some other task.
pub fn exit_current() -> ! {
    SCHEDULER.with(|s| s.exit_current())
}

/// Park the calling task as Blocked. The caller is responsible for making
/// sure `unblock(pid)` is eventually called.
pub fn block_current() {
    SCHEDULER.with(|s| s.block_current());
}

/// Wake a previously-blocked task.
pub fn unblock(pid: Pid) -> Result<(), SchedError> {
    SCHEDULER.with(|s| s.unblock(pid))
}

/// Timer-IRQ hook. Decrements the running task's time slice and preempts on
/// exhaustion.
///
/// The IRQ entry stub must save all volatile registers before calling this
/// function — `context_switch` only saves the callee-saved set.
pub fn tick() {
    SCHEDULER.with(|s| s.tick());
}

/// PID of the task currently on-CPU, if any.
pub fn current_pid() -> Option<Pid> {
    SCHEDULER.with(|s| s.current_pid())
}
