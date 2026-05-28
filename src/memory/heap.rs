//! Kernel heap.
//!
//! A first-fit linked-list free-list allocator. The free list lives
//! inside the heap region itself: each free block starts with a
//! `FreeBlock` header (size + next pointer), so there is no metadata
//! cost beyond `sizeof(FreeBlock)` per free block.
//!
//! Registered as the crate's `#[global_allocator]`, so `alloc::*` (Box,
//! Vec, BTreeMap, …) becomes available as soon as `init` returns.
//!
//! Known limitations (see `README.md` § "Heap"):
//! - No automatic block coalescing on `dealloc`. Long-lived workloads
//!   with mixed-size allocations will fragment.
//! - No heap-growth path. Once the initial 8 MiB is exhausted, alloc
//!   returns null and the global-alloc OOM handler fires.
//! - Worst-case `O(blocks)` allocation. Acceptable for a kernel heap
//!   that primarily allocates a small number of long-lived objects.

use core::alloc::{GlobalAlloc, Layout};
use core::mem;
use core::ptr;

use super::allocator;
use super::paging::{self, PageFlags};
use super::{MemoryInitError, SpinMutex, HEAP_SIZE, HEAP_START, PAGE_SIZE};

/// Header at the start of every free block. The header is overwritten
/// when the block is handed out; on `dealloc` a new header is written
/// into the returned region.
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

pub struct LinkedListAllocator {
    head: *mut FreeBlock,
}

// SAFETY: LinkedListAllocator owns the heap's free list. The only raw
// pointer it holds (`head`) points into the heap region, which is alive
// for the lifetime of the kernel. Synchronisation is enforced by the
// outer SpinMutex.
unsafe impl Send for LinkedListAllocator {}

impl LinkedListAllocator {
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
        }
    }

    /// Initialise the allocator with a single free block covering the
    /// whole heap region.
    ///
    /// # Safety
    /// `start..start+size` must be a writable, page-mapped, exclusively-
    /// owned virtual range; `start` must be aligned to
    /// `align_of::<FreeBlock>()`, and `size` must be at least
    /// `size_of::<FreeBlock>()`.
    unsafe fn init(&mut self, start: usize, size: usize) {
        self.head = ptr::null_mut();
        // SAFETY: contract on `init`. Range is exclusively ours.
        unsafe {
            self.add_free(start, size);
        }
    }

    /// Push a free block onto the head of the list.
    ///
    /// # Safety
    /// `addr` must be aligned for `FreeBlock`, the region
    /// `addr..addr+size` must be writable and not currently part of the
    /// free list, and `size >= size_of::<FreeBlock>()`.
    unsafe fn add_free(&mut self, addr: usize, size: usize) {
        debug_assert!(size >= mem::size_of::<FreeBlock>());
        debug_assert!(addr % mem::align_of::<FreeBlock>() == 0);
        let block = addr as *mut FreeBlock;
        // SAFETY: contract on `add_free` — region is writable and
        // exclusively ours for the duration of this write.
        unsafe {
            (*block).size = size;
            (*block).next = self.head;
        }
        self.head = block;
    }

    /// Walk the free list, first-fit. Returns the allocated address or
    /// null if no block is large enough.
    ///
    /// `size` is already rounded to `align_of::<FreeBlock>()` granularity
    /// and `align` is at least `align_of::<FreeBlock>()`.
    unsafe fn alloc_inner(&mut self, size: usize, align: usize) -> *mut u8 {
        let mut prev: *mut *mut FreeBlock = &mut self.head;
        // SAFETY: we walk a singly-linked list whose nodes were all
        // produced by `add_free`, so each `*prev` is either null or a
        // valid `FreeBlock` in the heap region.
        unsafe {
            while !(*prev).is_null() {
                let region = *prev;
                let region_addr = region as usize;
                let region_size = (*region).size;
                let region_end = region_addr + region_size;

                let alloc_start = align_up(region_addr, align);
                let Some(alloc_end) = alloc_start.checked_add(size) else {
                    prev = &mut (*region).next;
                    continue;
                };

                if alloc_end > region_end {
                    prev = &mut (*region).next;
                    continue;
                }

                let excess = region_end - alloc_end;
                // If the tail piece is smaller than a header it cannot
                // become a free block, and we would leak alignment.
                // Refuse this region and move on; the next one may have
                // a kinder layout.
                if excess > 0 && excess < mem::size_of::<FreeBlock>() {
                    prev = &mut (*region).next;
                    continue;
                }

                // Same check for the prefix produced by alignment.
                let prefix = alloc_start - region_addr;
                if prefix > 0 && prefix < mem::size_of::<FreeBlock>() {
                    prev = &mut (*region).next;
                    continue;
                }

                // Unlink the region.
                *prev = (*region).next;

                // Re-publish the prefix (if any) as a free block.
                if prefix > 0 {
                    self.add_free(region_addr, prefix);
                }
                // Re-publish the suffix (if any) as a free block.
                if excess > 0 {
                    self.add_free(alloc_end, excess);
                }

                return alloc_start as *mut u8;
            }
        }
        ptr::null_mut()
    }

    /// Return a block of `size` bytes to the free list.
    ///
    /// No coalescing — see module docs.
    ///
    /// # Safety
    /// `ptr` must have been returned by a prior `alloc_inner` with the
    /// same `(size, align)` reconstitution.
    unsafe fn dealloc_inner(&mut self, ptr: *mut u8, size: usize) {
        // SAFETY: the contract says `ptr` is a live allocation of at
        // least `size` bytes that is no longer in use, so writing a
        // FreeBlock header at its head is safe.
        unsafe {
            self.add_free(ptr as usize, size);
        }
    }

    /// Normalise a `Layout` for our allocator: round size up so the
    /// allocation can be re-used as a FreeBlock on free, and lift the
    /// alignment to at least `align_of::<FreeBlock>()`.
    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(mem::align_of::<FreeBlock>())
            .expect("alignment too large")
            .pad_to_align();
        let size = core::cmp::max(layout.size(), mem::size_of::<FreeBlock>());
        (size, layout.align())
    }
}

// SAFETY: the `SpinMutex` serialises all allocator state mutations, and
// `LinkedListAllocator` is `Send`. Therefore concurrent calls to
// `alloc`/`dealloc` see consistent state.
unsafe impl GlobalAlloc for SpinMutex<LinkedListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = LinkedListAllocator::size_align(layout);
        let mut a = self.lock();
        // SAFETY: under the lock; size/align are normalised.
        unsafe { a.alloc_inner(size, align) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = LinkedListAllocator::size_align(layout);
        let mut a = self.lock();
        // SAFETY: caller of `dealloc` follows GlobalAlloc's contract:
        // `ptr` was returned by a previous `alloc` with the same Layout.
        unsafe { a.dealloc_inner(ptr, size) }
    }
}

#[global_allocator]
static ALLOCATOR: SpinMutex<LinkedListAllocator> = SpinMutex::new(LinkedListAllocator::new());

/// Map heap pages and arm the global allocator.
///
/// # Safety
/// May be called only once, after `paging::init` and `allocator::init`.
pub unsafe fn init() -> Result<(), MemoryInitError> {
    // Map every page of the heap to a freshly-allocated frame.
    let mut mapped: usize = 0;
    while mapped < HEAP_SIZE {
        let virt = (HEAP_START + mapped) as u64;
        let frame = match allocator::alloc_frame() {
            Some(f) => f,
            None => return Err(MemoryInitError::HeapMappingFailed),
        };
        // SAFETY: `virt` is a fresh higher-half page never previously
        // mapped; the frame was just handed out by the allocator and
        // is exclusively ours. Flags grant kernel R/W and forbid
        // execution (the heap holds data only).
        let result = unsafe {
            paging::map(
                virt,
                frame.start_address(),
                PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
            )
        };
        if result.is_err() {
            return Err(MemoryInitError::HeapMappingFailed);
        }
        mapped += PAGE_SIZE;
    }

    // Now that the heap region is backed by real memory, hand it to
    // the allocator as one big free block.
    let mut a = ALLOCATOR.lock();
    // SAFETY: the entire HEAP_START..HEAP_START+HEAP_SIZE range is
    // mapped writable, exclusively owned by the allocator, and aligned
    // to PAGE_SIZE (which exceeds align_of::<FreeBlock>()).
    unsafe {
        a.init(HEAP_START, HEAP_SIZE);
    }

    Ok(())
}

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}
