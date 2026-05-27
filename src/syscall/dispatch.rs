//! Syscall dispatcher.
//!
//! `entry.asm` translates the Linux x86_64 register ABI into the SysV C ABI
//! and calls [`syscall_dispatch`] with the syscall number plus six argument
//! registers. This module routes that call to the correct Tier 1 handler.
//!
//! The dispatch table is a fixed-size array indexed by syscall number. This
//! mirrors how Linux organizes `sys_call_table` and keeps dispatch to a
//! single bounds check plus an indirect call — no hash, no match cascade.
//!
//! Return value convention: handlers return [`SyscallResult`]; we convert to
//! the Linux convention where negative values are `-errno` and non-negative
//! values are successes. The kernel side reasons in `Result`; the boundary
//! flattens to `i64` exactly once, here.

use crate::syscall::handlers;

/// Number of entries reserved in the dispatch table.
///
/// Sized to cover every Linux x86_64 syscall number Tier 1 might grow into
/// (Linux currently uses up to ~450). Unused slots return `-ENOSYS`.
pub const SYSCALL_TABLE_SIZE: usize = 512;

/// Linux `errno` values gkern uses. Values match `/usr/include/asm-generic/errno-base.h`.
#[repr(i64)]
#[derive(Copy, Clone, Debug)]
pub enum Errno {
    EPERM = 1,
    ENOENT = 2,
    EINTR = 4,
    EIO = 5,
    EBADF = 9,
    ECHILD = 10,
    ENOMEM = 12,
    EACCES = 13,
    EFAULT = 14,
    EBUSY = 16,
    EEXIST = 17,
    ENOTDIR = 20,
    EISDIR = 21,
    EINVAL = 22,
    ENFILE = 23,
    EMFILE = 24,
    ENOSYS = 38,
}

/// Result type every Tier 1 handler returns.
pub type SyscallResult = Result<i64, Errno>;

/// Function pointer signature for handlers in the dispatch table.
type Handler = fn(a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> SyscallResult;

/// Linux x86_64 syscall numbers we implement at Tier 1.
///
/// Sourced from `arch/x86/entry/syscalls/syscall_64.tbl` in the Linux tree.
/// Numbers MUST match Linux exactly — that is the whole point of ABI
/// compatibility.
pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const FORK: u64 = 57;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
}

/// Dispatch table, built once at boot.
///
/// `static mut` is acceptable here because: (a) the table is written exactly
/// once before any AP comes online, (b) it is read-only thereafter, and
/// (c) handler pointers are themselves Send+Sync. A future hardening pass
/// can move this into a `.rodata` table emitted at build time.
static mut SYSCALL_TABLE: [Handler; SYSCALL_TABLE_SIZE] = [enosys; SYSCALL_TABLE_SIZE];

/// Fallback handler for unimplemented syscall numbers.
fn enosys(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(Errno::ENOSYS)
}

/// Wires Tier 1 handlers into the dispatch table.
///
/// Called once during kernel bring-up, before SYSCALL is enabled on any CPU.
pub fn init() {
    // Safety: see comment on SYSCALL_TABLE — single-writer bring-up phase.
    unsafe {
        SYSCALL_TABLE[nr::READ as usize] = handlers::sys_read;
        SYSCALL_TABLE[nr::WRITE as usize] = handlers::sys_write;
        SYSCALL_TABLE[nr::OPEN as usize] = handlers::sys_open;
        SYSCALL_TABLE[nr::CLOSE as usize] = handlers::sys_close;
        SYSCALL_TABLE[nr::FORK as usize] = handlers::sys_fork;
        SYSCALL_TABLE[nr::EXECVE as usize] = handlers::sys_execve;
        SYSCALL_TABLE[nr::EXIT as usize] = handlers::sys_exit;
    }
}

/// Entry point called from `entry.asm`.
///
/// Uses the SysV C ABI: nr in rdi, args in rsi/rdx/rcx/r8/r9, plus arg5 on
/// the stack. `extern "C"` makes that mapping explicit so the assembly side
/// can rely on it.
///
/// Returns `i64` directly — Linux's negative-errno convention. Userspace
/// reads this from `rax` after SYSRET.
#[no_mangle]
pub extern "C" fn syscall_dispatch(
    nr: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
) -> i64 {
    let handler = if (nr as usize) < SYSCALL_TABLE_SIZE {
        // Safety: SYSCALL_TABLE is read-only after init().
        unsafe { SYSCALL_TABLE[nr as usize] }
    } else {
        enosys
    };

    match handler(a0, a1, a2, a3, a4, a5) {
        Ok(v) => v,
        Err(e) => -(e as i64),
    }
}
