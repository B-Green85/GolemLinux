//! x86_64 4-level paging.
//!
//! Virtual address layout used by the walker:
//! ```text
//!   63        48 47   39 38   30 29   21 20   12 11    0
//!  ┌─────────────┬───────┬───────┬───────┬───────┬───────┐
//!  │   sign-ext  │  L4   │  L3   │  L2   │  L1   │  off  │
//!  └─────────────┴───────┴───────┴───────┴───────┴───────┘
//! ```
//!
//! Bits 48..=63 must equal bit 47 (canonical-address form). Each level
//! has 512 8-byte entries → exactly one 4 KiB page per table.
//!
//! Physical addresses are read by going through the bootloader's
//! "physical memory offset" mapping: a contiguous higher-half region
//! that maps every byte of physical RAM. Recording the offset once at
//! `init` lets the walker dereference a frame's physical address as
//! `(offset + phys) as *mut PageTable`. No recursive entry, no fixed
//! aperture — just the offset map.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use super::allocator;
use super::MemoryInitError;
use super::SpinMutex;

/// Mask isolating the physical-frame address inside a PTE.
/// Bits 12..=51 hold the 4 KiB-aligned physical frame address; bits 0..=11
/// and 52..=62 hold flags; bit 63 is NX.
const PHYS_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Page table entry flags. Plain newtype so we don't pull in `bitflags`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFlags(u64);

impl PageFlags {
    pub const PRESENT: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const USER: Self = Self(1 << 2);
    pub const WRITE_THROUGH: Self = Self(1 << 3);
    pub const NO_CACHE: Self = Self(1 << 4);
    pub const ACCESSED: Self = Self(1 << 5);
    pub const DIRTY: Self = Self(1 << 6);
    pub const HUGE_PAGE: Self = Self(1 << 7);
    pub const GLOBAL: Self = Self(1 << 8);
    pub const NO_EXECUTE: Self = Self(1 << 63);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u64) -> Self {
        Self(bits & !PHYS_ADDR_MASK)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl core::ops::BitOrAssign for PageFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl core::ops::BitAnd for PageFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A single 64-bit page table entry.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_present(&self) -> bool {
        (self.0 & 1) != 0
    }

    pub const fn frame_addr(&self) -> u64 {
        self.0 & PHYS_ADDR_MASK
    }

    pub const fn flags(&self) -> PageFlags {
        PageFlags(self.0 & !PHYS_ADDR_MASK)
    }

    pub fn set(&mut self, frame_addr: u64, flags: PageFlags) {
        debug_assert!(
            frame_addr & !PHYS_ADDR_MASK == 0,
            "frame addr must be 4 KiB aligned and fit in 40 bits"
        );
        self.0 = (frame_addr & PHYS_ADDR_MASK) | flags.bits();
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

/// A page table. 512 8-byte entries → 4096 bytes, naturally page-aligned.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); 512],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The target virtual address already has a present mapping.
    AlreadyMapped,
    /// The target virtual address has no mapping.
    NotMapped,
    /// Ran out of physical frames while creating an intermediate table.
    OutOfFrames,
    /// Hit a HUGE_PAGE entry where we needed to descend further.
    HugePageConflict,
    /// `init` has not yet been called.
    NotInitialized,
}

/// Physical-memory offset, as recorded by `init`. Read on every walk.
/// `u64::MAX` is a sentinel for "not yet initialised" — chosen because
/// no valid offset can be all-ones (bit 63 must be 1 for higher-half
/// but bit 0 must be 0 because of 4 KiB alignment).
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(u64::MAX);

/// Serialises page-table mutation. Reads (translate) are lock-free —
/// the worst case is observing a torn 8-byte entry, which on x86_64
/// would only happen if two threads were *racing* a write, which our
/// mutex prevents.
static MAP_LOCK: SpinMutex<()> = SpinMutex::new(());

/// Record the bootloader's physical-memory offset.
///
/// # Safety
/// `physical_memory_offset` must be the virtual address of a writable
/// mapping that covers every byte of physical RAM the kernel may need
/// to touch (in particular every frame returned by the bootloader's
/// memory map). Must be called before any other function in this
/// module.
pub unsafe fn init(physical_memory_offset: u64) -> Result<(), MemoryInitError> {
    if physical_memory_offset == u64::MAX {
        return Err(MemoryInitError::PagingNotReady);
    }
    PHYSICAL_MEMORY_OFFSET.store(physical_memory_offset, Ordering::SeqCst);
    Ok(())
}

fn phys_offset() -> Result<u64, MapError> {
    let v = PHYSICAL_MEMORY_OFFSET.load(Ordering::SeqCst);
    if v == u64::MAX {
        Err(MapError::NotInitialized)
    } else {
        Ok(v)
    }
}

/// Translate a physical address to the offset-mapped virtual address.
fn phys_to_virt(phys: u64) -> Result<u64, MapError> {
    Ok(phys_offset()? + phys)
}

/// Read CR3 and return a mutable reference to the active L4 table.
///
/// # Safety
/// There is only one L4 per address space, but Rust cannot see that.
/// Callers must serialise modifications through `MAP_LOCK`.
unsafe fn active_l4() -> Result<*mut PageTable, MapError> {
    let cr3: u64;
    // SAFETY: `mov from cr3` is a read of a control register. It has no
    // memory side-effects and cannot fault in ring 0. We declare no
    // memory clobber because nothing observable in memory changes.
    asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    let pa = cr3 & PHYS_ADDR_MASK;
    let va = phys_to_virt(pa)?;
    Ok(va as *mut PageTable)
}

/// Decompose a canonical virtual address into table indices and the
/// page offset.
const fn split_indices(virt: u64) -> (usize, usize, usize, usize, u64) {
    let l4 = ((virt >> 39) & 0x1FF) as usize;
    let l3 = ((virt >> 30) & 0x1FF) as usize;
    let l2 = ((virt >> 21) & 0x1FF) as usize;
    let l1 = ((virt >> 12) & 0x1FF) as usize;
    let off = virt & 0xFFF;
    (l4, l3, l2, l1, off)
}

/// Walk to an existing next-level table or allocate + zero one.
///
/// `parent_flags` is used to compute the flags placed on the *new*
/// intermediate entry — we propagate USER so that user-space mappings
/// remain reachable, but we always set PRESENT+WRITABLE on an
/// intermediate (per-entry permissions still gate the final mapping).
unsafe fn next_table_or_create(
    table: &mut PageTable,
    index: usize,
    parent_flags: PageFlags,
) -> Result<&mut PageTable, MapError> {
    let entry = &mut table.entries[index];

    if entry.is_present() {
        if entry.flags().contains(PageFlags::HUGE_PAGE) {
            return Err(MapError::HugePageConflict);
        }
    } else {
        let frame = allocator::alloc_frame().ok_or(MapError::OutOfFrames)?;
        let new_table_pa = frame.start_address();
        let new_table_va = phys_to_virt(new_table_pa)? as *mut PageTable;

        // SAFETY: `new_table_va` is the offset-mapped virtual address of
        // a freshly-allocated, exclusively-owned physical frame. The
        // frame is properly aligned (frames are 4 KiB; PageTable is
        // 4 KiB-aligned). Writing a `PageTable::empty()` zeroes its
        // 512 entries before the entry below makes it reachable from
        // hardware.
        unsafe {
            new_table_va.write(PageTable::empty());
        }

        let intermediate_flags = PageFlags::PRESENT
            | PageFlags::WRITABLE
            | (parent_flags & PageFlags::USER);
        entry.set(new_table_pa, intermediate_flags);
    }

    let pa = entry.frame_addr();
    let va = phys_to_virt(pa)?;
    // SAFETY: the entry is present and not huge, so it points to a
    // valid next-level PageTable. The offset-mapped VA is alive for as
    // long as the offset map is — which is forever, in our model.
    unsafe { Ok(&mut *(va as *mut PageTable)) }
}

unsafe fn next_table(table: &PageTable, index: usize) -> Result<&mut PageTable, MapError> {
    let entry = &table.entries[index];
    if !entry.is_present() {
        return Err(MapError::NotMapped);
    }
    if entry.flags().contains(PageFlags::HUGE_PAGE) {
        return Err(MapError::HugePageConflict);
    }
    let va = phys_to_virt(entry.frame_addr())?;
    // SAFETY: the entry points to a live next-level table; the offset
    // mapping makes that table reachable at `va`.
    unsafe { Ok(&mut *(va as *mut PageTable)) }
}

/// Install a virtual-to-physical mapping for one 4 KiB page.
///
/// `PRESENT` is forced on. The caller is responsible for picking
/// WRITABLE / NO_EXECUTE / USER appropriately for the mapping.
///
/// # Safety
/// Mapping memory is intrinsically dangerous: the caller must ensure
/// the resulting alias does not violate Rust's aliasing rules for any
/// outstanding `&mut`. For page tables and frame-allocator internals
/// the caller must additionally hold no live reference into the region.
pub unsafe fn map(virt: u64, phys: u64, flags: PageFlags) -> Result<(), MapError> {
    assert!(virt & 0xFFF == 0, "virt must be page-aligned");
    assert!(phys & 0xFFF == 0, "phys must be page-aligned");

    let flags = flags | PageFlags::PRESENT;
    let (l4i, l3i, l2i, l1i, _) = split_indices(virt);

    let _guard = MAP_LOCK.lock();

    // SAFETY: the lock serialises all mutators. The CR3 read inside
    // `active_l4` is harmless; the subsequent dereferences are valid
    // because the offset map is live and the L4 is a real page table.
    let l4_table = unsafe { &mut *active_l4()? };
    let l3_table = unsafe { next_table_or_create(l4_table, l4i, flags)? };
    let l2_table = unsafe { next_table_or_create(l3_table, l3i, flags)? };
    let l1_table = unsafe { next_table_or_create(l2_table, l2i, flags)? };

    let entry = &mut l1_table.entries[l1i];
    if entry.is_present() {
        return Err(MapError::AlreadyMapped);
    }
    entry.set(phys, flags);

    // SAFETY: invlpg invalidates the TLB entry for the given linear
    // address. It has no effect beyond that and cannot fault in ring 0.
    unsafe {
        flush_tlb(virt);
    }
    Ok(())
}

/// Remove a 4 KiB mapping and return the physical frame that backed it.
///
/// # Safety
/// The caller must guarantee that no live Rust reference still refers
/// to the unmapped page.
pub unsafe fn unmap(virt: u64) -> Result<u64, MapError> {
    assert!(virt & 0xFFF == 0, "virt must be page-aligned");
    let (l4i, l3i, l2i, l1i, _) = split_indices(virt);

    let _guard = MAP_LOCK.lock();

    // SAFETY: under the map lock; offset map is live; tables are real.
    let l4_table = unsafe { &mut *active_l4()? };
    let l3_table = unsafe { next_table(l4_table, l4i)? };
    let l2_table = unsafe { next_table(l3_table, l3i)? };
    let l1_table = unsafe { next_table(l2_table, l2i)? };

    let entry = &mut l1_table.entries[l1i];
    if !entry.is_present() {
        return Err(MapError::NotMapped);
    }
    let pa = entry.frame_addr();
    entry.clear();

    // SAFETY: see `map` — pure TLB invalidation.
    unsafe {
        flush_tlb(virt);
    }
    Ok(pa)
}

/// Walk the active page tables to resolve `virt` to a physical address.
///
/// Reads only — does not take the map lock. The result reflects the
/// state of the tables at some instant during the walk; concurrent
/// mappers are blocked from publishing torn entries by the architecture
/// (64-bit aligned stores are atomic on x86_64).
pub fn translate(virt: u64) -> Option<u64> {
    let (l4i, l3i, l2i, l1i, off) = split_indices(virt);
    let offset = match PHYSICAL_MEMORY_OFFSET.load(Ordering::SeqCst) {
        u64::MAX => return None,
        v => v,
    };

    // SAFETY: the offset map is live (init invariant); CR3 read is
    // side-effect-free; we only ever produce shared `&` references to
    // page tables here, which is fine even if a mutator is writing —
    // x86_64 8-byte aligned reads/writes are atomic.
    unsafe {
        let cr3: u64;
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        let l4 = &*((offset + (cr3 & PHYS_ADDR_MASK)) as *const PageTable);
        let e4 = &l4.entries[l4i];
        if !e4.is_present() {
            return None;
        }

        let l3 = &*((offset + e4.frame_addr()) as *const PageTable);
        let e3 = &l3.entries[l3i];
        if !e3.is_present() {
            return None;
        }
        if e3.flags().contains(PageFlags::HUGE_PAGE) {
            // 1 GiB page: low 30 bits of `virt` are the offset.
            return Some(e3.frame_addr() | (virt & ((1u64 << 30) - 1)));
        }

        let l2 = &*((offset + e3.frame_addr()) as *const PageTable);
        let e2 = &l2.entries[l2i];
        if !e2.is_present() {
            return None;
        }
        if e2.flags().contains(PageFlags::HUGE_PAGE) {
            // 2 MiB page: low 21 bits of `virt` are the offset.
            return Some(e2.frame_addr() | (virt & ((1u64 << 21) - 1)));
        }

        let l1 = &*((offset + e2.frame_addr()) as *const PageTable);
        let e1 = &l1.entries[l1i];
        if !e1.is_present() {
            return None;
        }
        Some(e1.frame_addr() | off)
    }
}

/// Invalidate the TLB entry for a single page.
///
/// # Safety
/// `virt` is a virtual address. `invlpg` has no architectural side
/// effects beyond TLB invalidation; this is only `unsafe` because it
/// is an inline-asm.
#[inline]
unsafe fn flush_tlb(virt: u64) {
    // SAFETY: invlpg with a memory operand: invalidates the TLB entry
    // for the given linear address. We declare a memory clobber so the
    // compiler treats subsequent accesses to `virt` as fresh.
    asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
}

/// Flush the entire TLB by reloading CR3 with its current value.
///
/// # Safety
/// Same as `flush_tlb`. Expensive — prefer per-page flushes.
pub unsafe fn flush_tlb_all() {
    let cr3: u64;
    // SAFETY: reading CR3 is a privileged but side-effect-free move in
    // ring 0; `nomem` is correct because no memory is touched.
    asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    // SAFETY: writing CR3 back with its current value reloads the page-
    // table base unchanged, whose only architectural effect is flushing
    // every non-global TLB entry — exactly the intent of this function.
    // The active L4 is unchanged, so no mapping is altered.
    asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
}
