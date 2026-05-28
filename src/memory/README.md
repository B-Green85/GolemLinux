# `src/memory/` — Memory subsystem

Owner: Agent 2 of 6 (memory).
Target: x86_64, 4-level paging, UEFI boot (per `GOLEM_RUNTIME_ARCHITECTURE.md`).

This subsystem brings up:

1. **Physical frame allocator** (`allocator.rs`)
2. **Virtual memory / page tables** (`paging.rs`)
3. **Kernel heap** (`heap.rs`)
4. **Public interface** (`mod.rs`)

The entry point is `memory::init(MemoryInfo)`. After it returns, the
kernel has:

- A working frame allocator (`memory::allocator::alloc_frame`)
- A working page-mapping API (`memory::paging::map / unmap / translate`)
- A working `#[global_allocator]` — `alloc::*` collections work

---

## Public surface

```text
memory::init(MemoryInfo) -> Result<(), MemoryInitError>

memory::PAGE_SIZE                          usize  = 4096
memory::HEAP_START                         usize  = 0xFFFF_C000_0000_0000
memory::HEAP_SIZE                          usize  = 8 MiB

memory::allocator::Frame                   newtype around u64
memory::allocator::alloc_frame() -> Option<Frame>
unsafe memory::allocator::dealloc_frame(Frame)
memory::allocator::stats() -> (total, free)

memory::paging::PageFlags                  newtype: PRESENT / WRITABLE / USER /
                                                    NO_EXECUTE / HUGE_PAGE / ...
unsafe memory::paging::map(virt, phys, flags) -> Result<(), MapError>
unsafe memory::paging::unmap(virt)              -> Result<u64, MapError>
memory::paging::translate(virt)                 -> Option<u64>
unsafe memory::paging::flush_tlb_all()
```

`heap.rs` exports no public API beyond registering `#[global_allocator]`
and `init()` — the heap is meant to be invisible after boot.

---

## Boot-time contract with the boot module

`memory::init` takes a `MemoryInfo`:

```rust
pub struct MemoryInfo<'a> {
    pub physical_memory_offset: u64,
    pub regions: &'a [MemoryRegion],
}
```

Two things the boot module must guarantee before calling `init`:

1. **Offset mapping is live.** All physical RAM is mapped writable at
   `physical_memory_offset`. We dereference frame addresses through this
   mapping when walking page tables.
2. **`regions` is honest.** Any range marked `Usable` is genuinely RAM
   that nothing else cares about — not MMIO, not the kernel image, not
   bootloader scratch. We will hand pieces of these regions out as page
   tables and as heap pages.

We pessimistically refuse to allocate above `MAX_PHYSICAL_BYTES` (32 GiB
by default — see "Decisions" below).

---

## Decisions

### D1 — Bitmap frame allocator (one bit per 4 KiB frame, static)

Alternatives considered: bump pointer, linked-list free-list, buddy.

We chose a bitmap because:

- It is the simplest design that supports **deallocation**. A bump
  allocator can't free; we will need to free frames (page-table
  teardown, process exit), so we need at least a free-list-class design.
- It is **bounded in metadata**: 1 MiB of `.bss` covers 32 GiB of RAM
  (one bit per 4 KiB frame, 8 M frames, 8 M / 8 = 1 MiB). No external
  metadata storage problem.
- Allocation is **O(words)** in the worst case, which on modern x86_64
  is dominated by the L1 cache walk — fast enough for boot, fast enough
  for steady-state until we have measured pressure.

Trade-off: above `MAX_PHYSICAL_BYTES` we silently lose frames. We
return `MemoryInitError::BitmapTooSmall` so the caller knows. Raising
the cap is a one-line constant change at the cost of `.bss`. Going
beyond ~128 GiB-class machines means re-evaluating: at that scale the
bitmap walk starts to matter and a buddy or zoned allocator is more
defensible.

### D2 — Offset-mapped physical memory, no recursive entry

Two standard ways to read page tables you don't own:

- **Recursive entry**: install a self-referencing entry at L4[N] so that
  the virtual address `0xFFFF_<NNN>_<NNN>_<NNN>_xxx` walks back to your
  own tables. Powerful, but it complicates address-space switching and
  costs you one L4 slot.
- **Offset map**: have the bootloader map all of physical RAM at a fixed
  higher-half virtual base. Then "the virtual address of physical
  address P" = `offset + P`.

We use the offset map. Reasons:

- Most modern x86_64 bootloaders (the `bootloader` crate, Limine, etc.)
  already do this. Cheap to assume.
- Reading another address space's tables is just "switch CR3, walk via
  offset" — no need to install/uninstall a recursive entry.
- The implementation is shorter and harder to get wrong.

The recorded offset lives in a single `AtomicU64`. Sentinel value
`u64::MAX` means "init has not yet run" — chosen because no valid
higher-half offset can be all-ones.

### D3 — Page mapping is mutex-serialised; translation is lock-free

`paging::map` and `paging::unmap` take a `SpinMutex<()>` so we never
write an entry while another CPU is walking it.

`paging::translate` does **not** take the lock. x86_64 guarantees that
8-byte aligned reads/writes are atomic, so concurrent translation
either observes the pre-update or post-update PTE — never a torn one.

Caveat: this is not interrupt-safe. An interrupt handler that tries to
take the lock will deadlock if the interrupted code already holds it.
Once the kernel-wide interrupt-disable primitive lands (Agent 3 /
Agent 4) the lock should be wrapped accordingly. Until then,
`memory::init` must run with interrupts disabled.

### D4 — Heap location at `0xFFFF_C000_0000_0000`, 8 MiB initial

The conventional kernel layout we adopt:

```text
0xFFFF_8000_0000_0000 .. 0xFFFF_BFFF_FFFF_FFFF   physical-memory offset map  (~64 TiB)
0xFFFF_C000_0000_0000 .. 0xFFFF_C000_007F_FFFF   kernel heap                  (8 MiB)
0xFFFF_C000_0080_0000 .. ...                     reserved for heap growth, vmalloc
0xFFFF_FFFF_8000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   kernel image                 (-2 GiB)
```

8 MiB is enough to bring Sentinel's slab tables up and run the early
boot allocators. Once a real virtual-memory area manager exists the
heap should grow on demand instead of being a fixed window.

### D5 — Linked-list first-fit kernel heap

Alternatives considered: bump (can't free), fixed-size slab, buddy,
external `linked_list_allocator` crate.

We wrote our own simple free-list because:

- The deliverable says no external dependencies (Rust only).
- The implementation is small enough to audit at a glance — ~150 lines.
- The kernel heap is **not** the hot path for typical allocations; per-
  subsystem slab caches will live above it eventually.

Known limitations, documented and accepted:

- **No coalescing.** Freeing a block adds it to the head of the list
  but does not merge with neighbouring free blocks. Long-running mixed-
  size workloads will fragment.
- **No growth.** Once 8 MiB is exhausted, `alloc` returns null. The
  global-alloc OOM handler (registered by the parent crate) fires.
- **First-fit, not best-fit.** Faster but slightly more fragmenting.

These are all "replace before production" items, not blockers for
bringing the kernel up.

### D6 — All page table entries are written through one PTE type

A single `PageTableEntry(u64)` newtype carries both flags and the
frame address, with a `PHYS_ADDR_MASK = 0x000F_FFFF_FFFF_F000` that
isolates the 40 physical address bits. Intermediate (non-leaf) entries
always carry `PRESENT | WRITABLE` regardless of leaf flags — the
per-leaf flags do the actual permission gating. This matches the
Intel SDM's recommendation and avoids "I set the leaf writable but the
intermediate wasn't" foot-guns.

### D7 — `NO_EXECUTE` is set on heap pages

The heap holds data. There is no reason it should ever be executable.
We set `NO_EXECUTE` on every heap mapping so a write-where bug into the
heap cannot directly turn into code execution. (This relies on the
bootloader having enabled `EFER.NXE`. The boot module is expected to
do this; if it hasn't, the bit is harmlessly ignored.)

### D8 — `SpinMutex` lives in this module

We need a lock before the heap is up, and we cannot depend on `alloc`
or on Agent 4's future synchronisation crate. A minimal test-and-set
spinlock is inlined into `mod.rs`. It is `pub(crate)` so other modules
in this crate can use it, but it is not part of the memory subsystem's
public surface — replace it with the kernel-wide primitive when one
lands.

---

## Virtual address space layout (canonical x86_64 4-level)

```text
0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   user-space (lower half)
                                                 — gap of non-canonical addrs —
0xFFFF_8000_0000_0000 .. 0xFFFF_BFFF_FFFF_FFFF   physical-memory offset map
0xFFFF_C000_0000_0000 .. 0xFFFF_C000_007F_FFFF   kernel heap (initial 8 MiB)
0xFFFF_C000_0080_0000 .. 0xFFFF_FFFF_7FFF_FFFF   reserved (vmalloc, future)
0xFFFF_FFFF_8000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   kernel image
```

48-bit canonical-address rule: bits 48..=63 are sign-extended from
bit 47.

---

## Init order

`memory::init` performs, in order:

1. `paging::init(offset)` — records the physical-memory offset.
2. `allocator::init(regions)` — populates the bitmap from the memory map.
3. `heap::init()` — allocates frames, maps the heap region, arms the
   global allocator.

The order is load-bearing: each step depends on the prior one. `init`
takes `MemoryInfo` by value and is `unsafe` — see its docstring.

---

## Safety notes

Every `unsafe` block in this subsystem carries a comment that explains
which invariant lets us soundly perform the operation. The big ones:

- The offset map is **always live** once `paging::init` has run. We
  treat the boot module as having handed us a permanent capability.
- A frame returned by `allocator::alloc_frame` is **exclusively owned**
  by the caller — the bitmap will not return the same frame twice until
  it has been `dealloc`'d.
- `paging::map` and `paging::unmap` are **mutually exclusive** with each
  other via `MAP_LOCK`. `translate` reads without the lock and relies
  on x86_64's 8-byte atomic read/write guarantee.
- Page-table writes are followed by an `invlpg` for the affected page.
  We do not currently TLB-shootdown across CPUs — that becomes Agent 4's
  problem when SMP arrives.

---

## Things that are deliberately not here

- **TLB shootdown across CPUs** — punted to SMP bring-up.
- **Demand paging / page faults** — punted to Agent 3 (interrupts).
- **`vmalloc`-style discontiguous-physical, contiguous-virtual ranges**
  — easy to add on top of `paging::map` later.
- **Slab/slub caches** — go on top of the global heap.
- **Memory zones (DMA, low, high)** — single zone until a driver
  surfaces a real constraint.
- **Per-CPU magazines** — premature optimisation.
- **Memory cgroups / accountability** — Sentinel's territory, not the
  allocator's.

---

## Constraints honoured

- Rust only — no external crate dependencies, only `core` + the
  upcoming `alloc` (post-`heap::init`).
- x86_64, 4-level paging — hard-coded in `paging.rs`.
- No directory other than `src/memory/` touched.
- Every `unsafe` block has a comment explaining why it is sound.
- Each file lands in its own descriptive git commit.

---

*Copyright (c) 2026 TrueSystems LLC.*
