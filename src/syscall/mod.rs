//! Syscall subsystem — the user/kernel boundary.
//!
//! Layered as:
//!
//! ```text
//!     userspace
//!         | SYSCALL instruction
//!         v
//!     entry.rs            (global_asm! trampoline: register save, ABI
//!         |                translation, stack switch + Rust MSR programming)
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
//! and result types other subsystems will produce. The dispatch table itself,
//! the CPU entry trampoline, and the individual handlers stay internal.
//!
//! # `no_std`
//!
//! This subsystem uses only `core` (no `std`, no `alloc`) — see the audit note
//! in `README.md`. The `#![cfg_attr(not(test), no_std)]` below makes that
//! intent explicit and lets the module's unit tests run on the host with
//! `std`. The *authoritative* crate-level `#![no_std]` lives at the crate root
//! (`src/main.rs`, owned by Agent 7, Integration); a crate-level attribute on
//! a non-root module is otherwise inert. The hard guarantee is the absence of
//! `std::` usage here, not this attribute.

#![cfg_attr(not(test), no_std)]

mod entry;
pub mod dispatch;
pub mod handlers;

pub use dispatch::{nr, Errno, SyscallResult};

/// Single bring-up entry point for the syscall subsystem.
///
/// Agent 7 calls this from `kernel_main` (after the scheduler is initialized,
/// per the integration order). Wiring order *within* this call is fixed:
///   1. Install Tier 1 handlers in the dispatch table.
///   2. Program the SYSCALL/SYSRET MSRs on this CPU (writes `IA32_LSTAR` with
///      the address of the entry trampoline via `wrmsr`).
///
/// Step 1 must complete before step 2 so the very first `SYSCALL` after the
/// MSRs are armed sees a fully populated table.
///
/// # Privilege and CPU state
///
/// The MSR programming in step 2 uses `wrmsr`/`rdmsr`, which are **CPL 0-only**
/// (they `#GP` in userspace) and assume the CPU is already in 64-bit long mode.
/// Both hold inside `kernel_main`. See [`entry::init_cpu`] for the full
/// contract.
///
/// # Safety
///
/// Must be called exactly once per CPU during bring-up, at CPL 0 in long mode,
/// with interrupts disabled, after the GDT and per-CPU `GS` base are valid.
/// Calling it twice on the same CPU is harmless but wasteful; calling it before
/// the per-CPU area is set will fault the first time a syscall arrives.
pub unsafe fn init() {
    dispatch::init();
    entry::init_cpu();
}
