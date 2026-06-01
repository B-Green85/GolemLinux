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
//!
//! ## `no_std`
//!
//! This subsystem is `no_std`: it links only `core` (and, once
//! [`heap::init`] has run, `alloc`). It must never reach for `std` —
//! there is no operating system underneath us to provide one. A sweep
//! of every file here confirms only `core::*` paths are imported.
//!
//! The crate-level `#![no_std]` attribute itself is declared **once at
//! the crate root** (`src/main.rs`, owned by the integration agent), not
//! here: Rust ignores `#![no_std]` in any non-root module and warns
//! "the `#![no_std]` attribute can only be used at the crate root", so
//! repeating it in this `mod.rs` would add a warning without changing
//! behaviour. See `README.md` § "no_std".

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

/// Borrow-checked view of the memory map consumed by [`init_from_info`].
/// [`init`] reconstructs one of these from the raw [`BootHandoff`].
pub struct MemoryInfo<'a> {
    /// Virtual address at which all physical memory is identity-mapped by
    /// the bootloader. The paging module uses this to read/write page
    /// tables without setting up its own recursive entry.
    pub physical_memory_offset: u64,
    pub regions: &'a [MemoryRegion],
}

/// C-ABI boot→memory handoff block — the concrete thing the raw
/// `*const ()` handed to [`init`] points at.
///
/// The boot module (Agent 1) builds one of these, the firmware/boot
/// path passes its address to `kernel_main` in `rdi`, and integration
/// (Agent 7) forwards that address verbatim to [`init`]. It is the
/// stable, `#[repr(C)]` wire format across the asm↔Rust boundary; the
/// borrow-checked [`MemoryInfo`] is reconstructed from it inside `init`.
#[repr(C)]
pub struct BootHandoff {
    /// Must equal [`BOOT_HANDOFF_MAGIC`]. Lets [`init`] reject a stray,
    /// stale, or zeroed pointer before trusting any other field.
    pub magic: u64,
    /// Virtual base at which all physical RAM is mapped writable.
    /// See [`MemoryInfo::physical_memory_offset`].
    pub physical_memory_offset: u64,
    /// Number of [`MemoryRegion`] records at `regions`.
    pub region_count: u64,
    /// Pointer to `region_count` contiguous, properly-aligned
    /// `#[repr(C)]` [`MemoryRegion`]s describing the physical address
    /// space. May be null only when `region_count == 0`.
    pub regions: *const MemoryRegion,
}

/// Magic identifying a valid [`BootHandoff`]. ASCII `"GLMMEM01"`.
pub const BOOT_HANDOFF_MAGIC: u64 = 0x474C_4D4D_454D_3031;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryInitError {
    NoUsableRegions,
    BitmapTooSmall,
    HeapMappingFailed,
    AlreadyInitialized,
    PagingNotReady,
}

/// Integration entry point — bring the memory subsystem online from the
/// raw boot-handoff pointer that `kernel_main` receives.
///
/// This is the function the integration agent (Agent 7) calls from
/// `kernel_main`:
///
/// ```ignore
/// // memory_map: *const () — the rdi argument from boot.asm
/// memory::init(memory_map)?;
/// ```
///
/// It validates the [`BootHandoff`] the pointer refers to, reconstructs
/// a [`MemoryInfo`], runs the structured [`init_from_info`] bring-up,
/// and flattens any [`MemoryInitError`] to a `&'static str` so the
/// caller can report it without depending on this module's error enum.
///
/// The signature is intentionally safe so it drops straight into
/// `kernel_main`, but the operation is only *sound* under a caller
/// contract this function cannot check:
///
/// - `memory_map` must be either null (rejected with an error) or a
///   live, correctly-aligned [`BootHandoff`] whose `regions` /
///   `region_count` describe a valid array;
/// - it must be called exactly once, very early in boot, with
///   interrupts disabled.
///
/// Both are guaranteed by the boot/integration contract — see
/// `README.md` § "Boot-time contract with the boot module".
pub fn init(memory_map: *const ()) -> Result<(), &'static str> {
    if memory_map.is_null() {
        return Err("memory::init: null boot handoff pointer");
    }

    // SAFETY: per the caller contract the pointer refers to a live,
    // correctly-aligned BootHandoff. We form a shared reference and read
    // `magic` first; every other field is only trusted once the magic
    // check below has passed, so a zeroed or stale block is rejected
    // before we act on its contents.
    let handoff = unsafe { &*(memory_map as *const BootHandoff) };

    if handoff.magic != BOOT_HANDOFF_MAGIC {
        return Err("memory::init: bad boot handoff magic");
    }

    let region_count = handoff.region_count as usize;
    let regions: &[MemoryRegion] = if region_count == 0 {
        // A zero-length slice must not be built from a (possibly null)
        // raw pointer — use a genuinely empty slice instead.
        &[]
    } else if handoff.regions.is_null() {
        return Err("memory::init: null region array with non-zero count");
    } else {
        // SAFETY: the contract guarantees `regions` points to
        // `region_count` contiguous, aligned, `#[repr(C)]`
        // MemoryRegion records that outlive this call. MemoryRegion is
        // `Copy`, so a shared slice over them introduces no aliasing
        // hazard for the duration of init.
        unsafe { core::slice::from_raw_parts(handoff.regions, region_count) }
    };

    let info = MemoryInfo {
        physical_memory_offset: handoff.physical_memory_offset,
        regions,
    };

    // SAFETY: forwarded under the very same "call once, early, IRQs
    // disabled" contract documented above; `info` was just built from
    // the validated handoff block.
    unsafe { init_from_info(info) }.map_err(memory_init_error_str)
}

/// Render a [`MemoryInitError`] as a static, printable string for the
/// `&'static str` boundary that [`init`] exposes to integration.
const fn memory_init_error_str(err: MemoryInitError) -> &'static str {
    match err {
        MemoryInitError::NoUsableRegions => {
            "memory::init: no usable RAM regions in the memory map"
        }
        MemoryInitError::BitmapTooSmall => {
            "memory::init: physical RAM exceeds MAX_PHYSICAL_BYTES; raise the cap"
        }
        MemoryInitError::HeapMappingFailed => {
            "memory::init: failed to map the kernel heap (out of frames?)"
        }
        MemoryInitError::AlreadyInitialized => {
            "memory::init: memory subsystem already initialised"
        }
        MemoryInitError::PagingNotReady => {
            "memory::init: invalid physical_memory_offset in boot handoff"
        }
    }
}

/// Bring the memory subsystem online from a structured [`MemoryInfo`].
///
/// This is the workhorse. Integration calls the raw-pointer [`init`]
/// wrapper instead; this entry stays public so callers that already
/// hold a typed `MemoryInfo` (or tests) can drive bring-up directly.
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
pub unsafe fn init_from_info(info: MemoryInfo<'_>) -> Result<(), MemoryInitError> {
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
