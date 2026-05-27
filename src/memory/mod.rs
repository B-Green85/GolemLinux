//! Memory subsystem for the Golem kernel (gkern).
//!
//! Three concerns live here:
//!
//! 1. [`allocator`] — the physical frame allocator. Owns the bitmap that
//!    tracks which 4 KiB physical frames are free.
//! 2. [`paging`]    — page table walking and mapping. Wraps the active
//!    x86_64 4-level hierarchy and exposes `map` / `unmap` / `translate`.
//! 3. [`heap`]      — the kernel heap. Maps a contiguous virtual region
//!    backed by frames from the frame allocator and exposes it as the
//!    `#[global_allocator]`.
//!
//! Architectural decisions are documented in `README.md`.

#![allow(dead_code)]

pub mod allocator;
pub mod heap;
pub mod paging;

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Size of a physical frame and a virtual page in bytes.
pub const PAGE_SIZE: usize = 4096;

/// Virtual start of the kernel heap.
///
/// Placed in the higher-half above the direct physical-memory window so
/// it does not collide with the kernel image, stack, or offset map.
/// See `README.md` § "Virtual address space layout".
pub const HEAP_START: usize = 0xFFFF_C000_0000_0000;

/// Initial size of the kernel heap (8 MiB). Sufficient for early boot and
/// the slab tables Sentinel builds up before userspace comes online.
pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

/// Description of one contiguous range in the physical address space, as
/// reported by the bootloader/firmware. Owned by the boot module; the
/// memory subsystem just consumes a slice.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub start: u64,
    pub end: u64,
    pub kind: MemoryRegionKind,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryRegionKind {
    /// Free, RAM-backed, fair game for the frame allocator.
    Usable,
    /// Firmware / MMIO / ACPI / etc. Untouchable.
    Reserved,
    /// Holds the kernel image (text/rodata/data/bss). Must be preserved.
    KernelImage,
    /// Bootloader scratch — preserved until handoff is complete.
    Bootloader,
}

/// Handoff structure passed from the boot module to [`init`].
pub struct MemoryInfo<'a> {
    /// Virtual address at which all physical memory is identity-mapped by
    /// the bootloader. The paging module uses this to read/write page
    /// tables without setting up its own recursive entry.
    pub physical_memory_offset: u64,
    pub regions: &'a [MemoryRegion],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryInitError {
    NoUsableRegions,
    BitmapTooSmall,
    HeapMappingFailed,
    AlreadyInitialized,
    PagingNotReady,
}

/// Bring the memory subsystem online.
///
/// Order is load-bearing: paging needs the offset before any page-table
/// walk happens; the frame allocator must be populated before the heap
/// can map any pages; the heap must be mapped before any `alloc::*`
/// container is constructed.
///
/// # Safety
/// Must be called exactly once, very early in boot, with interrupts
/// disabled. `info.regions` must accurately describe usable physical
/// RAM, and `info.physical_memory_offset` must be a writable mapping of
/// the entire physical address range used by the system.
pub unsafe fn init(info: MemoryInfo<'_>) -> Result<(), MemoryInitError> {
    // SAFETY: caller's contract — first thing we do is record the offset
    // so the page-table walker can dereference frame addresses. Stores
    // no state beyond an atomic.
    paging::init(info.physical_memory_offset)?;

    // SAFETY: caller's contract — `regions` is a valid slice describing
    // RAM. `allocator::init` only mutates the static bitmap; it does not
    // touch the described frames yet.
    allocator::init(info.regions)?;

    // SAFETY: by this point both the frame allocator and the page-table
    // walker are usable, which is what `heap::init` requires.
    heap::init()?;

    Ok(())
}

/// Round `addr` up to the next multiple of `align`. `align` must be a
/// power of two.
#[inline]
pub const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// Round `addr` down to the previous multiple of `align`. `align` must
/// be a power of two.
#[inline]
pub const fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}

// ---------------------------------------------------------------------
// Internal spinlock
// ---------------------------------------------------------------------
//
// The memory subsystem cannot depend on the eventual kernel-wide lock
// crate (that's Agent 4's territory) and we cannot use `alloc` before
// the heap is up. So we ship a minimal test-and-set spinlock here.
// Replace with the kernel-wide primitive once the synchronisation story
// is settled.

pub(crate) struct SpinMutex<T> {
    locked: AtomicBool,
    inner: UnsafeCell<T>,
}

// SAFETY: SpinMutex mediates access to T such that at most one thread
// holds a `&mut T` at a time, so `T: Send` is sufficient for the mutex
// itself to be Send + Sync.
unsafe impl<T: Send> Send for SpinMutex<T> {}
unsafe impl<T: Send> Sync for SpinMutex<T> {}

impl<T> SpinMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free. No fairness, no
    /// priority inheritance, no IRQ masking. Adequate for short critical
    /// sections that do not nest.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Relaxed read while waiting avoids hammering the cache line
            // with RMW operations.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

pub(crate) struct SpinGuard<'a, T> {
    lock: &'a SpinMutex<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding a SpinGuard proves we own the lock; therefore
        // we have exclusive access for the lifetime of the borrow.
        unsafe { &*self.lock.inner.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same as `deref` — exclusive access is guaranteed by
        // the outstanding guard.
        unsafe { &mut *self.lock.inner.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
