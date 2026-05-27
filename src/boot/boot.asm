; ============================================================================
; Golem Linux — UEFI Bootloader Entry (x86_64)
; ============================================================================
; File:      src/boot/boot.asm
; Subsystem: Bootloader (Agent 1)
; Format:    PE32+ UEFI application
; Assembler: NASM 2.15+ (Intel syntax)
;
; Copyright (c) 2026 TrueSystems LLC. All rights reserved.
;
; ----------------------------------------------------------------------------
; Purpose
; ----------------------------------------------------------------------------
;
; This is the single Assembly translation unit in the Golem bootloader. It is
; the first instruction stream to execute after UEFI firmware hands control
; to us; its only responsibility is to receive that handoff and transfer
; control to the Rust entry point `bootloader_main` with the UEFI arguments
; intact. Everything substantive happens in Rust above this file.
;
; The contract on CPU state, paging, GDT/IDT, interrupts, calling convention,
; and stack alignment at the moment `efi_main` is invoked is documented in
; README.md §"UEFI Handoff Assumptions". The short version:
;
;   • CPU is in 64-bit long mode; paging is on and identity-mapped.
;   • Microsoft x64 calling convention is in effect (matches Rust `efiapi`).
;       RCX = EFI_HANDLE        ImageHandle
;       RDX = EFI_SYSTEM_TABLE *SystemTable
;   • Stack is 16-byte aligned such that RSP + 8 ≡ 0 (mod 16) on entry.
;   • Direction flag is clear; interrupts are enabled.
;
; This file deliberately does NOT touch interrupts, the GDT, the IDT, the
; control registers, paging, or the stack. All of those are firmware's
; responsibility until ExitBootServices() returns inside Rust.
; ============================================================================

bits 64
default rel

global  efi_main
extern  bootloader_main

; ----------------------------------------------------------------------------
; .text — code section
; ----------------------------------------------------------------------------
section .text

; ----------------------------------------------------------------------------
; efi_main — UEFI application entry point
;
;   Equivalent C declaration:
;
;       EFI_STATUS EFIAPI efi_main(
;           EFI_HANDLE        ImageHandle,    ; RCX
;           EFI_SYSTEM_TABLE *SystemTable);   ; RDX
;
;   Equivalent Rust declaration (provided by the kernel-loader subsystem):
;
;       #[no_mangle]
;       pub extern "efiapi" fn bootloader_main(
;           image_handle: EfiHandle,
;           system_table: *mut EfiSystemTable,
;       ) -> EfiStatus;
;
;   Because `efiapi` IS Microsoft x64, RCX and RDX already hold the values
;   firmware placed in them; we forward them untouched.
; ----------------------------------------------------------------------------
efi_main:
    push    rbp                 ; frame pointer for boot-stage stack walking
    mov     rbp, rsp

    sub     rsp, 32             ; MS x64 shadow space for bootloader_main's
                                ; downstream calls; preserves 16-byte align

    ; Arguments are already correctly placed for `extern "efiapi"`.
    call    bootloader_main

    ; bootloader_main returns only on a path that did NOT call
    ; ExitBootServices(); UEFI is therefore still in scope and will read the
    ; EFI_STATUS value Rust placed in RAX.
    mov     rsp, rbp
    pop     rbp
    ret
