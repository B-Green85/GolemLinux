//! Tier 1 syscall handlers.
//!
//! Each handler implements the Linux x86_64 ABI for a single syscall number.
//! The signatures here match the SysV C ABI used by [`crate::syscall::dispatch`]:
//! six `u64` arguments, returning [`SyscallResult`]. Arguments unused by a
//! given syscall are simply ignored.
//!
//! These handlers are deliberately thin. They:
//!   1. Validate arguments at the user/kernel boundary.
//!   2. Hand off to the subsystem that actually performs the work (VFS,
//!      process manager, etc., owned by other agents).
//!   3. Translate subsystem errors back into Linux `errno` values.
//!
//! Where the corresponding subsystem is not yet wired in by another agent,
//! the handler returns `Err(Errno::ENOSYS)` rather than guessing at behavior.
//! This makes "not yet implemented" explicit and lets the rest of the kernel
//! make forward progress.

use crate::syscall::dispatch::{Errno, SyscallResult};

// ---------------------------------------------------------------------------
// External subsystem hooks
//
// These are owned by other agents (VFS, process manager). We declare the
// signatures we need so this file compiles standalone; the linker resolves
// them once the rest of gkern is present. If an agent has not yet provided
// a symbol, the corresponding handler will return ENOSYS via the cfg below.
// ---------------------------------------------------------------------------

#[cfg(feature = "vfs")]
extern "Rust" {
    fn vfs_read(fd: i32, buf: *mut u8, count: usize) -> Result<usize, Errno>;
    fn vfs_write(fd: i32, buf: *const u8, count: usize) -> Result<usize, Errno>;
    fn vfs_open(path: *const u8, flags: i32, mode: u32) -> Result<i32, Errno>;
    fn vfs_close(fd: i32) -> Result<(), Errno>;
}

#[cfg(feature = "process")]
extern "Rust" {
    fn proc_fork() -> Result<i32, Errno>;
    fn proc_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8)
        -> Result<core::convert::Infallible, Errno>;
    fn proc_exit(status: i32) -> !;
}

// ---------------------------------------------------------------------------
// Argument validation helpers
// ---------------------------------------------------------------------------

/// Highest legal user-mode virtual address on x86_64 with 4-level paging.
/// Anything at or above this is kernel space; a userspace pointer there is
/// either a bug or an attack.
const USER_VA_MAX: u64 = 0x0000_8000_0000_0000;

/// Reject pointers that are null or point into kernel space.
///
/// This is the bare minimum check before we trust a user pointer. The real
/// access-with-page-fault-handling lives in the memory subsystem (another
/// agent); this is just the cheap rejection at the boundary.
fn validate_user_ptr(ptr: u64, len: u64) -> Result<(), Errno> {
    if ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let end = ptr.checked_add(len).ok_or(Errno::EFAULT)?;
    if end > USER_VA_MAX {
        return Err(Errno::EFAULT);
    }
    Ok(())
}

/// Treat a `u64` argument as a file descriptor. Linux defines `fd` as `int`,
/// so values outside the i32 range are immediately invalid.
fn validate_fd(raw: u64) -> Result<i32, Errno> {
    if raw > i32::MAX as u64 {
        return Err(Errno::EBADF);
    }
    Ok(raw as i32)
}

// ---------------------------------------------------------------------------
// Handlers — signatures dictated by the dispatch table
// ---------------------------------------------------------------------------

/// `ssize_t read(int fd, void *buf, size_t count)` — Linux nr 0.
pub fn sys_read(fd: u64, buf: u64, count: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let fd = validate_fd(fd)?;
    validate_user_ptr(buf, count)?;

    #[cfg(feature = "vfs")]
    unsafe {
        return vfs_read(fd, buf as *mut u8, count as usize).map(|n| n as i64);
    }
    #[cfg(not(feature = "vfs"))]
    {
        let _ = fd;
        Err(Errno::ENOSYS)
    }
}

/// `ssize_t write(int fd, const void *buf, size_t count)` — Linux nr 1.
pub fn sys_write(fd: u64, buf: u64, count: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let fd = validate_fd(fd)?;
    validate_user_ptr(buf, count)?;

    #[cfg(feature = "vfs")]
    unsafe {
        return vfs_write(fd, buf as *const u8, count as usize).map(|n| n as i64);
    }
    #[cfg(not(feature = "vfs"))]
    {
        let _ = fd;
        Err(Errno::ENOSYS)
    }
}

/// `int open(const char *pathname, int flags, mode_t mode)` — Linux nr 2.
///
/// Linux retains `open(2)` (not just `openat(2)`) for x86_64 ABI compatibility,
/// so we implement it. Path-length validation is done by the VFS layer once
/// it walks the string; here we only reject obviously bad pointers.
pub fn sys_open(path: u64, flags: u64, mode: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // Length 1 = "at least one byte addressable"; VFS walks the NUL terminator.
    validate_user_ptr(path, 1)?;
    if flags > i32::MAX as u64 {
        return Err(Errno::EINVAL);
    }

    #[cfg(feature = "vfs")]
    unsafe {
        return vfs_open(path as *const u8, flags as i32, mode as u32).map(|fd| fd as i64);
    }
    #[cfg(not(feature = "vfs"))]
    {
        let _ = (flags, mode);
        Err(Errno::ENOSYS)
    }
}

/// `int close(int fd)` — Linux nr 3.
pub fn sys_close(fd: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let fd = validate_fd(fd)?;

    #[cfg(feature = "vfs")]
    unsafe {
        return vfs_close(fd).map(|_| 0);
    }
    #[cfg(not(feature = "vfs"))]
    {
        let _ = fd;
        Err(Errno::ENOSYS)
    }
}

/// `pid_t fork(void)` — Linux nr 57.
///
/// Linux internally implements `fork` as a `clone` with `SIGCHLD`; for Tier 1
/// we expose the raw `fork` number directly. The process manager handles the
/// COW + child-vs-parent return distinction.
pub fn sys_fork(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    #[cfg(feature = "process")]
    unsafe {
        return proc_fork().map(|pid| pid as i64);
    }
    #[cfg(not(feature = "process"))]
    Err(Errno::ENOSYS)
}

/// `int execve(const char *pathname, char *const argv[], char *const envp[])` — Linux nr 59.
///
/// On success this never returns to the caller — the calling process image
/// has been replaced. We model that with `Result<Infallible, Errno>` from the
/// subsystem; the `Ok` arm is unreachable, the `Err` arm produces an errno.
pub fn sys_execve(path: u64, argv: u64, envp: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    validate_user_ptr(path, 1)?;
    // argv and envp may be NULL on some Linux variants; tolerate that.
    if argv != 0 {
        validate_user_ptr(argv, core::mem::size_of::<usize>() as u64)?;
    }
    if envp != 0 {
        validate_user_ptr(envp, core::mem::size_of::<usize>() as u64)?;
    }

    #[cfg(feature = "process")]
    unsafe {
        match proc_execve(
            path as *const u8,
            argv as *const *const u8,
            envp as *const *const u8,
        ) {
            Ok(never) => match never {},
            Err(e) => Err(e),
        }
    }
    #[cfg(not(feature = "process"))]
    {
        let _ = (path, argv, envp);
        Err(Errno::ENOSYS)
    }
}

/// `void exit(int status)` — Linux nr 60. Does not return.
///
/// Linux's `exit(2)` terminates only the calling thread; full-process exit
/// is `exit_group(2)` (nr 231). Tier 1 implements the single-thread variant
/// to keep semantics identical to Linux for the same syscall number.
pub fn sys_exit(status: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    #[cfg(feature = "process")]
    unsafe {
        proc_exit(status as i32);
    }
    #[cfg(not(feature = "process"))]
    {
        let _ = status;
        Err(Errno::ENOSYS)
    }
}
