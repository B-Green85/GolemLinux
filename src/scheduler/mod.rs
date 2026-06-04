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

// no_std: every path in this subsystem resolves through `core` or `alloc`;
// `std` is never referenced (audited in Phase 2). The *authoritative*
// `#![no_std]` for the kernel lives in the crate root (`src/main.rs`, owned by
// the integration agent). This crate-level attribute mirrors the convention
// the sibling subsystems use (see `syscall/mod.rs`) and keeps the module
// std-free under `cargo test`, where `std` is linked for the test harness —
// hence the `not(test)` guard. As an inner attribute in a non-root module it
// is a no-op for the real kernel build; the no_std guarantee here is enforced
// by the imports, not by this line.
#![cfg_attr(not(test), no_std)]

pub mod context;
pub mod process;
pub mod scheduler;

pub use process::{
    alloc_pid, EntryFn, OwnerId, Pid, Process, ProcessState, PID_BOOTSTRAP, PID_IDLE,
};
pub use scheduler::{SchedError, Scheduler, DEFAULT_TIME_SLICE, SCHEDULER};

use alloc::boxed::Box;
use alloc::string::String;

/// Kernel stack size for the built-in idle task, in bytes. The idle loop does
/// nothing but `hlt`, so it needs no real working set; one 4 KiB page is the
/// recommended floor from `init_with` and leaves ABI headroom.
const IDLE_STACK_SIZE: usize = 4096;

/// Owner ID recorded for kernel-internal tasks (the bootstrap thread and the
/// built-in idle task) until Sentinel resolves the real kernel principal. Zero
/// is reserved for "the kernel itself" and is never handed to a user process.
pub const KERNEL_OWNER: OwnerId = 0;

/// Built-in idle entry point: halt until the next interrupt, forever.
///
/// Used by the zero-argument [`init`] so the integration layer does not have to
/// supply a power-management entry of its own. `extern "C"` and `-> !` to match
/// [`EntryFn`]. Callers that have their own idle/power-management routine can
/// bypass this by calling [`init_with`] instead.
extern "C" fn idle_loop() -> ! {
    loop {
        // SAFETY: `hlt` merely halts the CPU until the next interrupt. It has
        // no memory effects and is valid at CPL 0, where every kernel task
        // (idle included) runs. `nomem`/`nostack` reflect that it touches
        // neither; it leaves interrupts and flags untouched.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Initialize the scheduler — the entry point the integration layer calls from
/// `kernel_main`.
///
/// Takes no arguments: it allocates the idle task's kernel stack from the
/// global heap and installs the built-in [`idle_loop`] as the idle entry, with
/// [`KERNEL_OWNER`] as the placeholder accountability ID.
///
/// # Ordering dependency
///
/// **Memory must be initialized first.** This function allocates from the
/// global heap (the idle stack); calling it before `memory::init` has
/// registered the `#[global_allocator]` will fault. In `kernel_main` the order
/// is: `sentinel::init` → `memory::init` → … → `scheduler::init`.
///
/// Must be called exactly once. Installs the idle task and records the
/// currently-executing kernel thread as the bootstrap PCB so the first context
/// switch off it has somewhere to save state.
pub fn init() -> Result<(), SchedError> {
    // Allocated from the global heap — requires memory::init to have run.
    let idle_stack: Box<[u8]> = alloc::vec![0u8; IDLE_STACK_SIZE].into_boxed_slice();
    init_with(KERNEL_OWNER, idle_stack, idle_loop)
}

/// Initialize the scheduler with caller-supplied idle parameters.
///
/// The general form behind [`init`]. Use this when the kernel wants to provide
/// its own idle/power-management entry, a pre-allocated idle stack, or a
/// specific owner ID rather than the [`KERNEL_OWNER`] placeholder.
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
pub fn init_with(
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
