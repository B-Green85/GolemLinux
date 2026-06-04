/*
 * ============================================================================
 * Golem Linux — Kernel Linker Script
 * ============================================================================
 * File:      src/boot/linker.ld
 * Subsystem: Bootloader (Agent 1) — defines the kernel's load layout contract
 * Target:    x86_64-unknown-none, ELF64
 * Use:       passed to rust-lld / ld.lld / GNU ld when linking gkern:
 *
 *     ld --script src/boot/linker.ld -o build/gkern.elf <objects>
 *
 * Copyright (c) 2026 Brandon Green. Licensed under the Apache 2.0 License.
 *
 * ----------------------------------------------------------------------------
 * The Golem bootloader (a PE32+ UEFI application whose entry stub lives in
 * boot.asm of this directory) parses this ELF, copies each PT_LOAD segment
 * to the virtual address recorded here, zero-initialises .bss, installs page
 * tables matching the per-section permissions below, and jumps to the symbol
 * `kernel_main`. From that moment, UEFI Boot Services no longer exist.
 *
 * Kernel entry handoff ABI (Phase 2 contract — Agent 1 ⇄ Agent 7)
 * ---------------------------------------------------------------
 * The bootloader→kernel jump is a System V AMD64 control transfer, NOT the
 * Microsoft x64 (efiapi) convention used at the UEFI entry in boot.asm. The
 * Rust kernel ELF is built for x86_64-unknown-none, whose default ABI is
 * System V, so `kernel_main` is reached with:
 *
 *     RDI = pointer to the captured UEFI memory map (first argument)
 *
 * `kernel_main` is owned and defined by Agent 7 (src/main.rs); it is NOT
 * defined in this directory. This script only NAMES it as the ELF entry so
 * Agent 7's symbol is what the loader records in e_entry and what the
 * bootloader jumps to. Equivalent Rust declaration Agent 7 must provide:
 *
 *     #[no_mangle]
 *     pub extern "C" fn kernel_main(memory_map: *const ()) -> !;
 *
 * The opaque `*const ()` matches the memory subsystem's
 * `memory::init(memory_map: *const ()) -> Result<(), &'static str>` (Agent 2),
 * which `kernel_main` forwards RDI to. See README.md §"Phase 2 integration
 * review" for the full convention chain.
 *
 * Virtual address space layout
 * ----------------------------
 *
 *     0xFFFFFFFF80000000   ┌─────────────────────────────┐ ← KERNEL_BASE
 *                          │ .text       (R-X)           │
 *                          ├─────────────────────────────┤ 4 KiB aligned
 *                          │ .rodata     (R--)           │
 *                          ├─────────────────────────────┤ 4 KiB aligned
 *                          │ .data       (RW-)           │
 *                          ├─────────────────────────────┤ 4 KiB aligned
 *                          │ .bss        (RW-)           │
 *                          └─────────────────────────────┘ ← __kernel_image_end
 *
 * KERNEL_BASE is the canonical "negative 2 GiB" higher-half address. It is
 * the home of the x86_64 SysV "kernel" code model (`-C code-model=kernel` in
 * Rust, `-mcmodel=kernel` in C), permitting sign-extended 32-bit immediate
 * displacements throughout the kernel image. See README.md §"Kernel Memory
 * Layout" for the full rationale.
 *
 * Sections are 4 KiB-aligned so each may be mapped with distinct page-table
 * permissions, enforcing W^X (no page is simultaneously writable and
 * executable) — a required invariant for any kernel that runs untrusted
 * agents, which Golem explicitly does.
 * ============================================================================
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)

/*
 * Kernel ELF entry point. Renamed from `_kernel_start` to `kernel_main` in
 * Phase 2 to match the integration-agreed entry symbol (Agent 7, src/main.rs).
 * ASSUMPTION (must hold for boot to succeed): the kernel crate defines a
 * `#[no_mangle] pub extern "C" fn kernel_main(...)`, and its first-instruction
 * function is placed in `.text.boot` (KEEPt at the start of .text below) so
 * the bootloader can jump to KERNEL_BASE + headers without parsing e_entry.
 */
ENTRY(kernel_main)

KERNEL_BASE = 0xFFFFFFFF80000000;
PAGE_SIZE   = 0x1000;                 /* 4 KiB */

/*
 * Program headers: one PT_LOAD per permission class. The bootloader uses
 * exactly these three segments to drive page-table construction.
 *
 *   FLAGS bits (ELF64 PF_*):
 *       PF_X = 1, PF_W = 2, PF_R = 4
 */
PHDRS
{
    text   PT_LOAD FLAGS(5);          /* R + X */
    rodata PT_LOAD FLAGS(4);          /* R     */
    data   PT_LOAD FLAGS(6);          /* R + W */
}

SECTIONS
{
    . = KERNEL_BASE;
    __kernel_image_start = .;

    /* ---------------------------------------------------------------- *
     *  .text — kernel code                                             *
     *                                                                  *
     *  `.text.boot` is KEEPt at the very start of .text so a minimal   *
     *  trampoline (Rust or asm) lands at a deterministic offset from   *
     *  KERNEL_BASE, allowing the bootloader to jump there without      *
     *  parsing the ELF header's e_entry field.                         *
     * ---------------------------------------------------------------- */
    .text : ALIGN(PAGE_SIZE)
    {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        . = ALIGN(PAGE_SIZE);
        __text_end = .;
    } :text

    /* ---------------------------------------------------------------- *
     *  .rodata — constants, vtables, embedded blobs                    *
     *                                                                  *
     *  .data.rel.ro is the Rust/LLVM section for "read-only after      *
     *  relocations applied"; in a kernel that is never relocated past  *
     *  link time, this is effectively rodata.                          *
     * ---------------------------------------------------------------- */
    .rodata : ALIGN(PAGE_SIZE)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
        . = ALIGN(PAGE_SIZE);
        __rodata_end = .;
    } :rodata

    /* ---------------------------------------------------------------- *
     *  .data — initialised mutable globals                             *
     * ---------------------------------------------------------------- */
    .data : ALIGN(PAGE_SIZE)
    {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.plt)
        . = ALIGN(PAGE_SIZE);
        __data_end = .;
    } :data

    /* ---------------------------------------------------------------- *
     *  .bss — zero-initialised mutable globals                         *
     *                                                                  *
     *  Sized in-file as 0 bytes; the bootloader zeroes the range       *
     *  [__bss_start, __bss_end) after copying the PT_LOAD segment.     *
     * ---------------------------------------------------------------- */
    .bss : ALIGN(PAGE_SIZE)
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(PAGE_SIZE);
        __bss_end = .;
    } :data

    __kernel_image_end = .;

    /* ---------------------------------------------------------------- *
     *  Discarded: artefacts the kernel does not use at runtime.        *
     *                                                                  *
     *  The kernel panics by aborting (no unwinding), is statically     *
     *  linked, and has no interpreter — so unwind tables, dynamic      *
     *  linking metadata, and ELF notes are dead weight.                *
     * ---------------------------------------------------------------- */
    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.dynamic)
        *(.dynsym)
        *(.dynstr)
        *(.hash) *(.gnu.hash)
        *(.interp)
        *(.gnu.version*)
    }
}
