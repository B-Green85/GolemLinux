//! Sentinel subsystem.
//!
//! Sentinel is Golem's host-side trust authority. Agents running on the host
//! register with it across a kernel-mediated channel before they are granted a
//! permission tier. This module groups the kernel-side pieces of that system.
//!
//! Phase 4 / Agent 1 deliverable: [`ipc`] — the virtio-serial IPC channel that
//! carries the registration handshake between the Golem guest kernel and the
//! host Sentinel daemon. See that module's docs for the wire protocol and the
//! VirtIO driver bring-up.
//!
//! Phase 5 / Agent 3 deliverable: [`migration`] — the boot-time configuration
//! migration daemon. It replays any Safe-Mode-authored Sentinel configuration
//! from the fixed migration buffer (owned by `safemode::sentinel_config`, Agent
//! 2) into the live [`MONITOR`] before the scheduler starts. See that module.
//!
//! ## Kernel-resident pieces wired here
//!
//! Two long-standing Sentinel modules — [`audit`] (the append-only, SHA-256
//! chained audit log) and [`monitor`] (the passive degradation monitor that
//! owns the tunable [`monitor::Thresholds`]) — were authored in earlier phases
//! but were never declared in this facade, so they did not compile into the
//! kernel. Phase 5 needs both: the migration daemon applies thresholds through
//! [`MONITOR`] and records the change in [`AUDIT`], and Agent 2's
//! `safemode::sentinel_config` imports `sentinel::audit::sha256_hex` and
//! `sentinel::monitor::Thresholds`. They are declared below, and the
//! [`SpinLock`] they depend on (`crate::sentinel::SpinLock`) is defined here —
//! matching the synchronization primitive documented in `README.md` § 3.3.
//!
//! ## Boot-time init order (hard requirement)
//!
//! The integration crate root (`src/main.rs`) brings the subsystems up in this
//! order. Migration is **step 2 of the Sentinel bring-up**, but — see the note
//! on [`migration::run`] — it must run *after* `memory::init` because it both
//! allocates (checksum verification, audit append) and dereferences the mapped
//! migration page:
//!
//! ```text
//!   sentinel::init()            // invisibility gate / IPC channel — FIRST, always
//!   memory::init(memory_map)?   // heap online; migration page reachable
//!   sentinel::migration::run()? // check + apply Safe Mode config; halt on Err
//!   fs::init()
//!   scheduler::init()?
//!   syscall::init()
//! ```
//!
//! `sentinel::migration::run()` returns `Err` on an unresolved/forged Sentinel
//! configuration; the caller MUST halt the kernel rather than continue, because
//! booting with an unverified Sentinel config is not a recoverable state.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub mod audit;
pub mod ipc;
pub mod migration;
pub mod monitor;

// Convenience re-exports of the IPC entry points so callers can write
// `sentinel::init()` / `sentinel::handle_one()` without reaching into `ipc`.
pub use ipc::{
    build_response, handle_one, init, parse_register_request, read_request, serialize_response,
    write_response_bytes, IpcError, PermissionTier, RegisterRequest, RegisterResponse,
};

// ===========================================================================
// Global Sentinel state.
// ===========================================================================
//
// These two singletons are the kernel-resident Sentinel surface the rest of
// the kernel talks to in `Kernel` context. They are `const`-constructed into
// statics (no allocation at construction), so they exist from the first
// instruction — only their *use* needs the heap, which is why migration runs
// after `memory::init`.

/// The append-only, SHA-256-chained audit trail. Every consequential Sentinel
/// action lands here. The migration daemon records each applied/rejected
/// configuration migration into it (see [`migration`]).
pub static AUDIT: audit::AuditTrail = audit::AuditTrail::new();

/// The passive degradation monitor. It owns the live [`monitor::Thresholds`];
/// `MONITOR.set_thresholds(..)` is the **internal Sentinel configuration API**
/// the migration daemon applies Safe-Mode edits through — no socket, no IPC.
pub static MONITOR: monitor::Monitor = monitor::Monitor::new();

// ===========================================================================
// SpinLock — the subsystem's synchronization primitive (README.md § 3.3).
// ===========================================================================
//
// Sentinel runs on the syscall hot path and in early boot. Async needs a
// runtime we don't have; `std::sync::Mutex` needs `std` and would call the OS
// scheduler (circular for a subsystem the scheduler depends on). A spinlock is
// the right primitive: bounded critical sections, RAII guard, `AtomicBool` with
// `Acquire`/`Release` ordering, `core::hint::spin_loop()` while contended.
//
// `audit.rs` and `monitor.rs` both reference `crate::sentinel::SpinLock`; this
// is that type. `new` is `const` so it can back the `AUDIT` / `MONITOR` statics
// and the `const fn` constructors of `AuditTrail` / `Monitor`.

/// A minimal test-and-set spinlock with an RAII guard. `no_std`-only.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock mediates access so at most one `&mut T` (via the guard)
// exists at a time; `T: Send` is therefore sufficient for `SpinLock<T>` to be
// both `Send` and `Sync`.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Construct an unlocked spinlock. `const` so it can live in a `static`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free. No fairness, no IRQ
    /// masking — adequate for the short, non-nesting critical sections Sentinel
    /// holds it for.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Relaxed spin while contended avoids hammering the cache line with
            // read-modify-write traffic.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

/// RAII guard for [`SpinLock`]. Releases the lock on drop.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding the guard proves exclusive ownership of the lock for
        // the lifetime of this borrow.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same as `deref` — the outstanding guard guarantees exclusive
        // access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
