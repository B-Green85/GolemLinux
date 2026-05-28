//! Physical frame allocator.
//!
//! A bitmap allocator: one bit per 4 KiB physical frame, packed into
//! `u64` words. A set bit means "used"; a clear bit means "free". The
//! bitmap lives in `.bss` and is statically sized to cover the maximum
//! supported physical memory (`MAX_PHYSICAL_BYTES`).
//!
//! Rationale: a bitmap supports `dealloc`, gives O(words) allocation,
//! and is bounded in memory by a single `static`. A buddy allocator or
//! a per-zone free-list would be more efficient but is overkill until
//! we have measured pressure to justify the complexity. See `README.md`.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{
    align_down, align_up, MemoryInitError, MemoryRegion, MemoryRegionKind, SpinMutex, PAGE_SIZE,
};

/// Maximum supported physical RAM. 32 GiB chosen so the bitmap fits in
/// 1 MiB of `.bss`. Frames above this address are silently ignored —
/// raise this constant if the target hardware needs more.
pub const MAX_PHYSICAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;

const MAX_FRAMES: usize = (MAX_PHYSICAL_BYTES / PAGE_SIZE as u64) as usize;
const BITMAP_WORDS: usize = MAX_FRAMES / 64;

/// A 4 KiB physical frame, identified by its base address.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(u64);

impl Frame {
    pub const SIZE: u64 = PAGE_SIZE as u64;

    /// Containing-frame for a physical address.
    pub const fn containing_address(addr: u64) -> Frame {
        Frame(addr & !(Self::SIZE - 1))
    }

    /// Physical address of this frame's first byte.
    pub const fn start_address(self) -> u64 {
        self.0
    }

    /// Index of this frame in the bitmap.
    pub const fn number(self) -> u64 {
        self.0 / Self::SIZE
    }
}

/// Allocator interface — exposed so `paging` can take a `&mut dyn` if
/// it ever needs to allocate without going through the global lock.
pub trait FrameAllocator {
    fn allocate_frame(&mut self) -> Option<Frame>;
}

pub trait FrameDeallocator {
    /// # Safety
    /// `frame` must not be currently mapped or referenced. The caller
    /// is responsible for any TLB flushing that the unmap implied.
    unsafe fn deallocate_frame(&mut self, frame: Frame);
}

pub struct BitmapFrameAllocator {
    /// 1 = used, 0 = free. Frames not described by any memory region
    /// stay marked used so the rotating search never hands them out.
    bitmap: [u64; BITMAP_WORDS],
    /// Index of the bitmap word to start the next search at, so we
    /// don't always scan from zero.
    next_search: usize,
    total_frames: usize,
    free_frames: usize,
}

impl BitmapFrameAllocator {
    pub const fn new() -> Self {
        Self {
            bitmap: [u64::MAX; BITMAP_WORDS],
            next_search: 0,
            total_frames: 0,
            free_frames: 0,
        }
    }

    fn mark_free(&mut self, frame: Frame) {
        let i = frame.number() as usize;
        if i >= MAX_FRAMES {
            return;
        }
        let word = i / 64;
        let bit = i % 64;
        if self.bitmap[word] & (1u64 << bit) != 0 {
            self.bitmap[word] &= !(1u64 << bit);
            self.free_frames += 1;
        }
    }

    fn mark_used(&mut self, frame: Frame) {
        let i = frame.number() as usize;
        if i >= MAX_FRAMES {
            return;
        }
        let word = i / 64;
        let bit = i % 64;
        if self.bitmap[word] & (1u64 << bit) == 0 {
            self.bitmap[word] |= 1u64 << bit;
            self.free_frames -= 1;
        }
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    pub fn free_frames(&self) -> usize {
        self.free_frames
    }
}

impl FrameAllocator for BitmapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<Frame> {
        if self.free_frames == 0 {
            return None;
        }
        for offset in 0..BITMAP_WORDS {
            let i = (self.next_search + offset) % BITMAP_WORDS;
            let word = self.bitmap[i];
            if word != u64::MAX {
                let bit = (!word).trailing_zeros() as usize;
                self.bitmap[i] = word | (1u64 << bit);
                self.free_frames -= 1;
                self.next_search = i;
                let frame_index = (i * 64 + bit) as u64;
                return Some(Frame(frame_index * Frame::SIZE));
            }
        }
        None
    }
}

impl FrameDeallocator for BitmapFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: Frame) {
        // SAFETY contract delegated to caller — see trait docs. We just
        // flip the bit; we do not double-free check because the bitmap
        // ignores a clear-of-already-clear.
        self.mark_free(frame);
        // Bias future allocations toward this freed region so we tend
        // to re-use freshly-freed frames (better cache behaviour).
        let word = (frame.number() as usize) / 64;
        if word < BITMAP_WORDS {
            self.next_search = word;
        }
    }
}

// Global instance. The bitmap is 1 MiB in `.bss`, zeroed at link time,
// then overwritten to `u64::MAX` by `BitmapFrameAllocator::new()`'s
// const initialiser — meaning every frame starts out marked used until
// `init` clears the bits for ranges the bootloader marked usable.
static FRAME_ALLOCATOR: SpinMutex<BitmapFrameAllocator> =
    SpinMutex::new(BitmapFrameAllocator::new());

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Populate the bitmap from the bootloader-provided memory map.
///
/// # Safety
/// Must be called exactly once, after [`super::paging::init`], before
/// any frame allocation. `regions` must describe physical RAM honestly.
pub unsafe fn init(regions: &[MemoryRegion]) -> Result<(), MemoryInitError> {
    if INITIALIZED.swap(true, Ordering::AcqRel) {
        return Err(MemoryInitError::AlreadyInitialized);
    }

    let mut allocator = FRAME_ALLOCATOR.lock();

    let mut any_usable = false;
    for r in regions {
        if r.kind != MemoryRegionKind::Usable {
            continue;
        }
        let start = align_up(r.start, Frame::SIZE);
        let end = align_down(r.end, Frame::SIZE);
        if end <= start {
            continue;
        }
        any_usable = true;

        let mut addr = start;
        while addr + Frame::SIZE <= end {
            let frame = Frame(addr);
            let idx = frame.number() as usize;
            if idx >= MAX_FRAMES {
                // Out-of-range frame. Leave it marked used (it already is)
                // and report the bitmap as too small so the caller knows
                // they're losing memory above `MAX_PHYSICAL_BYTES`.
                return Err(MemoryInitError::BitmapTooSmall);
            }
            if allocator.bitmap[idx / 64] & (1u64 << (idx % 64)) != 0 {
                allocator.bitmap[idx / 64] &= !(1u64 << (idx % 64));
                allocator.free_frames += 1;
                allocator.total_frames += 1;
            }
            addr += Frame::SIZE;
        }
    }

    if !any_usable {
        return Err(MemoryInitError::NoUsableRegions);
    }

    Ok(())
}

/// Allocate a single physical frame, or `None` if exhausted.
pub fn alloc_frame() -> Option<Frame> {
    FRAME_ALLOCATOR.lock().allocate_frame()
}

/// Return a frame to the pool.
///
/// # Safety
/// `frame` must not be mapped into any address space and must not be
/// referenced by any live pointer. Any mapping that used it must have
/// already issued the corresponding TLB flush.
pub unsafe fn dealloc_frame(frame: Frame) {
    // SAFETY: forwarded to the inner allocator's `deallocate_frame`,
    // which has the same contract as documented above.
    FRAME_ALLOCATOR.lock().deallocate_frame(frame);
}

/// Snapshot of `(total_frames, free_frames)`.
pub fn stats() -> (usize, usize) {
    let a = FRAME_ALLOCATOR.lock();
    (a.total_frames, a.free_frames)
}
