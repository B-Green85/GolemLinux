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
 * Copyright (c) 2026 TrueSystems LLC. All rights reserved.
 *
 * ----------------------------------------------------------------------------
 * The Golem bootloader (a PE32+ UEFI application whose entry stub lives in
 * boot.asm of this directory) parses this ELF, copies each PT_LOAD segment
 * to the virtual address recorded here, zero-initialises .bss, installs page
 * tables matching the per-section permissions below, and jumps to the symbol
 * `_kernel_start`. From that moment, UEFI Boot Services no longer exist.
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
ENTRY(_kernel_start)

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
