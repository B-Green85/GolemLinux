# src/boot/ — Golem Linux Bootloader Subsystem

**Subsystem owner:** Agent 1 (bootloader)
**Scope:** every file in this directory; nothing outside it.

*Copyright (c) 2026 TrueSystems LLC. All rights reserved.*

---

This directory holds the Assembly entry point of the Golem bootloader and the
linker script for the Golem kernel. The Rust portion of the bootloader (the
code that walks the UEFI memory map, exits boot services, sets up paging,
loads the kernel ELF, and jumps to `_kernel_start`) belongs to a peer
subsystem and is intentionally absent here — these two files plus this
document are the bootloader subsystem's complete deliverable.

---

## Files

| File         | Role                                                        |
| ------------ | ----------------------------------------------------------- |
| `boot.asm`   | x86_64 NASM source: UEFI entry → Rust `bootloader_main`     |
| `linker.ld`  | GNU ld script: virtual layout of the kernel ELF             |
| `README.md`  | This file — every design decision in one place              |

`boot.asm` is linked into the **bootloader binary** (a PE32+ UEFI
application). `linker.ld` is the link script for the **kernel binary** (an
ELF64 file the bootloader loads). The two binaries are distinct artefacts
and intentionally so: see *§Why a separate bootloader binary*.

---

## Boot flow

```
┌──────────────┐    UEFI handoff                ┌──────────────────┐
│ UEFI firmware│ ─────────────────────────────→ │ efi_main         │
└──────────────┘  (RCX = ImageHandle,           │  (boot.asm)      │
                   RDX = SystemTable)           └────────┬─────────┘
                                                         │ call
                                                         ▼
                                               ┌──────────────────┐
                                               │ bootloader_main  │
                                               │ (Rust, efiapi)   │ ← not in
                                               └────────┬─────────┘   this dir
                                                        │ GetMemoryMap,
                                                        │ ExitBootServices,
                                                        │ build page tables,
                                                        │ copy gkern PT_LOAD
                                                        │ segments to
                                                        │ KERNEL_BASE,
                                                        │ zero .bss,
                                                        │ jump
                                                        ▼
                                               ┌──────────────────┐
                                               │ _kernel_start    │
                                               │ (gkern ELF,      │ ← not in
                                               │  linker.ld)      │   this dir
                                               └──────────────────┘
```

The two boundaries this subsystem **owns** are the first arrow (UEFI →
`efi_main`) and the contract `linker.ld` imposes on whatever produces the
kernel ELF. Every other arrow belongs to peer subsystems.

---

## UEFI handoff assumptions

The contract at the moment `efi_main` is entered is defined by the **UEFI
Specification 2.10 §2.3.4** ("x64 Platforms"). Golem makes the following
assumptions; if any of them are violated by a firmware implementation,
boot is undefined.

### CPU mode

- 64-bit long mode is active (`EFER.LMA = 1`, `CS.L = 1`, `CS.D = 0`).
- Protected mode and paging are on (`CR0.PE = 1`, `CR0.PG = 1`).
- Ring-0 write protection is enforced (`CR0.WP = 1`).
- SSE is enabled and exceptions go through `#XF`
  (`CR4.OSFXSR = 1`, `CR4.OSXMMEXCPT = 1`); we do not re-init the FPU.
- Direction flag (`RFLAGS.DF`) is **clear** — required by both UEFI and
  System V x86_64 ABIs.
- Interrupts are enabled (`RFLAGS.IF = 1`); CPU exception vectors are
  handled by firmware's IDT.

### Memory & paging

- Identity-mapped paging covers every region described by the UEFI memory
  map. Page granularity is firmware-chosen (typically 4 KiB or 2 MiB).
- Virtual address == physical address for every address we reach in
  `efi_main`; the bootloader's Rust half remains in identity-mapped memory
  until it installs its own page tables.
- The UEFI memory map itself is **not yet captured**. Capturing it is the
  Rust bootloader's first responsibility. Once `GetMemoryMap()` returns,
  that buffer becomes the single source of truth for physical memory, and
  every other assumption made above about the layout is discarded.

### GDT, IDT, TSS

- A valid GDT containing a 64-bit code segment (`CS`) and at least one
  data segment (`DS`/`ES`/`SS`) is installed and active.
- An IDT is installed; firmware handles exceptions and the periodic timer
  tick that drives `Stall()`/`SetTimer()`.
- We do **not** rely on segment bases — they are zero in long mode and
  our code is exclusively RIP-relative.

### Stack

- Firmware provides a stack of at least 128 KiB (per UEFI spec).
- `RSP` is 16-byte aligned **before** the `call` into `efi_main`; on
  entry, `RSP + 8 ≡ 0 (mod 16)` because `call` pushed the return address.
- 32 bytes of shadow space below the return address are reserved by the
  caller for our use, per Microsoft x64.

### Calling convention

UEFI uses Microsoft x64. Arguments to `efi_main`:

| Register | Value                              |
| -------- | ---------------------------------- |
| `RCX`    | `EFI_HANDLE ImageHandle`           |
| `RDX`    | `EFI_SYSTEM_TABLE *SystemTable`    |

Return value in `RAX`: `EFI_STATUS` (a `UINT64` where `0` means success
and the MSB indicates error).

Rust's `extern "efiapi"` matches Microsoft x64 exactly. `efi_main`
therefore performs no register shuffling — it forwards control directly.

---

## What `boot.asm` deliberately does *not* do

Each of the following is left to code that runs *after* `efi_main` returns
control to Rust (or, in some cases, after `ExitBootServices()` succeeds):

- **Does not disable interrupts.** UEFI Boot Services require them.
- **Does not reload the GDT or IDT.** UEFI's are valid until
  `ExitBootServices()` returns.
- **Does not touch `CR0`, `CR3`, `CR4`, or `EFER`.** Paging stays
  identity-mapped; control registers stay as firmware left them.
- **Does not switch stacks.** The UEFI-provided stack is sufficient for
  the bootloader stage; the kernel installs its own kernel stacks
  after handoff.
- **Does not zero `.bss`.** The bootloader binary's `.bss` is zeroed by
  the UEFI PE32+ loader as part of image load, before `efi_main` runs.

The minimum-touch design exists for one reason: **if `boot.asm` mutates
state, that mutation has to be undone before any UEFI Boot Service call
inside Rust will succeed.** Every saved instruction here is a class of
boot-time bug that cannot occur.

---

## Why a separate bootloader binary

There are two viable shapes for a UEFI-booted Rust kernel:

1. **Kernel-is-the-UEFI-app.** The kernel ELF (or PE32+) is itself the
   UEFI application. UEFI loads it; the kernel does its own
   `ExitBootServices` and higher-half transition.
2. **Separate bootloader.** A small UEFI application loads a kernel
   binary, sets up paging, exits boot services, and jumps to the kernel.

Golem uses option (2) for three reasons:

- **Higher-half virtual addressing.** UEFI loads PE32+ images into
  identity-mapped low memory. A higher-half kernel (`KERNEL_BASE =
  0xFFFFFFFF80000000`) cannot be the UEFI app directly — *something*
  has to construct the page tables that map it. That something is the
  bootloader.
- **Format mismatch.** PE32+ is appropriate for code that talks to UEFI;
  ELF64 is appropriate for code that doesn't. Keeping them separate
  lets each binary use the format that fits.
- **Recovery and update.** Safe Mode (per `GOLEM_RUNTIME_ARCHITECTURE.md`)
  is a separate boot environment. A standalone bootloader is the natural
  selection point between Standard, Hardened, and Safe Mode kernels;
  collapsing it into the kernel would force every recovery path through
  a kernel image.

---

## Kernel memory layout (`linker.ld`)

```
0xFFFFFFFF80000000  ┌─────────────────────────────┐ ← KERNEL_BASE
                    │ .text       (R-X)           │
                    ├─────────────────────────────┤ 4 KiB aligned
                    │ .rodata     (R--)           │
                    ├─────────────────────────────┤ 4 KiB aligned
                    │ .data       (RW-)           │
                    ├─────────────────────────────┤ 4 KiB aligned
                    │ .bss        (RW-)           │
                    └─────────────────────────────┘ ← __kernel_image_end
```

### Why `0xFFFFFFFF80000000`

- **Canonical "negative 2 GiB"** — top of the x86_64 48-bit canonical
  address space, minus 2 GiB. The address is sign-extended-canonical, so
  the MMU will not reject it.
- **Userspace stays contiguous in the low half.** Every userspace process
  keeps the entire lower 47 bits of virtual address space free, with
  no kernel mapping holes punching through the middle.
- **No CR3 switch on syscall.** The kernel mapping is the same in every
  process's top-half PML4 entries, so the syscall path does not have to
  reload page tables.
- **`code-model=kernel`.** Rust's `-C code-model=kernel` (LLVM's
  `CodeModel::Kernel`) emits sign-extended 32-bit immediate displacements
  for the negative-2 GiB region. Without it, the linker would issue 64-bit
  absolute relocations everywhere — far slower and far larger.

### Why 4 KiB section alignment

Each section ends at a 4 KiB boundary so the kernel can install distinct
page-table entries per section, enforcing **W^X** — no page is both
writable and executable:

| Section   | Page permission | Rationale                                  |
| --------- | --------------- | ------------------------------------------ |
| `.text`   | R-X             | Code is executed but never written.        |
| `.rodata` | R--             | Constants are neither written nor jumped.  |
| `.data`   | RW-             | Mutable globals are written, not executed. |
| `.bss`    | RW-             | Same.                                      |

W^X is **mandatory** for Golem: per
`GOLEM_RUNTIME_ARCHITECTURE.md`, agents are first-class processes and may
run untrusted inference code. A writable-executable kernel page in such an
environment is a Sentinel bypass surface.

### Exported symbols

Linker-defined symbols the kernel can declare in Rust as `extern "C"
static` to inspect at runtime. Their *address* is the boundary they name;
they have no value of their own.

| Symbol                  | Meaning                                   |
| ----------------------- | ----------------------------------------- |
| `__kernel_image_start`  | Lowest virtual address used by gkern.     |
| `__text_start/_end`     | Bounds of `.text`.                        |
| `__rodata_start/_end`   | Bounds of `.rodata`.                      |
| `__data_start/_end`     | Bounds of `.data`.                        |
| `__bss_start/_end`      | Bounds of `.bss` — bootloader zeroes this.|
| `__kernel_image_end`    | One past the highest used virtual byte.   |
| `_kernel_start`         | Kernel entry; defined by the kernel crate.|

### `.text.boot`

The script `KEEP`s any object's `.text.boot` section at the very start of
`.text` so a minimal trampoline lands at a deterministic offset from
`KERNEL_BASE`. The kernel crate is expected to place its first-instruction
function in that section; the bootloader can then jump to
`KERNEL_BASE + page_size_align(elf_headers)` without parsing `e_entry`.

### Discarded sections

`.eh_frame`, `.eh_frame_hdr`, `.note*`, `.comment`, `.dynamic`,
`.dynsym`, `.dynstr`, `.hash`, `.gnu.hash`, `.interp`, and `.gnu.version*`
are dropped: gkern does not unwind (panics abort), is statically linked,
and has no interpreter. Carrying these sections would only inflate the
image and the per-boot SHA256 measurement.

---

## Build expectations

| Tool                          | Minimum version  | Used for                          |
| ----------------------------- | ---------------- | --------------------------------- |
| NASM                          | 2.15             | assembling `boot.asm`             |
| `rust-lld` / `ld.lld` / GNU `ld` | LLVM 16 / 2.38 | linking against `linker.ld`       |
| Rust target `x86_64-unknown-uefi`   | nightly    | bootloader PE32+ output           |
| Rust target `x86_64-unknown-none`   | nightly    | kernel ELF output                 |

Building the bootloader and the kernel themselves is the responsibility of
peer subsystems; this directory defines the contracts, not the build
script.

---

## Constraints and open items

- **`bootloader_main` is not yet defined here.** It is a symbol resolved
  at link time, owned by the kernel-loader subsystem. Until that
  subsystem lands, `boot.asm` will fail to link with an undefined-symbol
  error — by design.
- **Page size is hard-coded at 4 KiB.** Huge-page mappings for `.text`
  and `.rodata` are deferred until the kernel MMU subsystem can express
  them.
- **`KERNEL_BASE` is fixed.** KASLR is a future concern; when it lands
  the entropy source will sit in the Rust bootloader, not this script.
- **No multiboot2 fallback.** Golem boots via UEFI only on the reference
  x86_64 target; community ports may need a different entry path.

---

## Sentinel note

Per *Three principles govern every design decision* in
`GOLEM_RUNTIME_ARCHITECTURE.md`, **every action has a hash**. The
bootloader runs before Sentinel exists, so the first audit-trail entry of
each boot — covering the bootloader image *and* the kernel image it
loaded — is written by the Rust bootloader the moment the kernel takes
over and Sentinel initialises. The two source files in this directory
contribute to that hash; their content is load-bearing for the audit
trail, not just for boot.
