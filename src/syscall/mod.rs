//! Syscall subsystem — the user/kernel boundary.
//!
//! Layered as:
//!
//! ```text
//!     userspace
//!         | SYSCALL instruction
//!         v
//!     entry.asm           (register save, ABI translation, stack switch)
//!         | extern "C" call
//!         v
//!     dispatch.rs         (table lookup, errno flattening)
//!         | fn pointer
//!         v
//!     handlers.rs         (Tier 1 syscalls, boundary validation)
//!         | calls into
//!         v
//!     other subsystems    (VFS, process manager, owned by other agents)
//! ```
//!
//! Public surface kept intentionally tiny: bring-up routines plus the error
//! and result types other subsystems will produce. The dispatch table itself
//! and the individual handlers stay internal.

#![cfg_attr(not(test), no_std)]

pub mod dispatch;
pub mod handlers;

pub use dispatch::{nr, Errno, SyscallResult};

extern "C" {
    /// Programs the SYSCALL/SYSRET MSRs on the current CPU.
    ///
    /// Defined in `entry.asm`. Must be called after the per-CPU GS base is
    /// set up (memory agent) and the GDT carries the kernel/user selectors
    /// referenced by the STAR value in `entry.asm`.
    fn syscall_init_msrs();
}

/// Single bring-up entry point for the syscall subsystem.
///
/// Wiring order is fixed:
///   1. Install Tier 1 handlers in the dispatch table.
///   2. Program SYSCALL MSRs on this CPU.
///
/// Step 1 must complete before step 2 so the very first SYSCALL after MSRs
/// are armed sees a fully populated table.
///
/// # Safety
///
/// Must be called exactly once per CPU during bring-up, with interrupts
/// disabled, after the GDT and per-CPU GS base are valid. Calling it twice
/// on the same CPU is harmless but wasteful; calling it before the per-CPU
/// area is set will fault the first time a syscall arrives.
pub unsafe fn init() {
    dispatch::init();
    syscall_init_msrs();
}
