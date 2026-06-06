//! gkern — Golem Linux kernel crate root (integration layer, Agent 7).
//!
//! This is the ONE crate root. It owns the crate-level attributes (`no_std`,
//! `no_main`), the global `extern crate alloc;`, the `#[panic_handler]`, and
//! the kernel entry point. The six subsystems live under `src/<subsystem>/`
//! and are owned by Agents 1–6. This file does NOT modify them — it only
//! declares the modules and wires their public `init` entry points together
//! in the required order.
//!
//! Init order (hard requirement from the integration spec):
//!   1. sentinel  — invisibility gate up before anything else
//!   2. memory    — heap up before anything allocates
//!   3. fs        — needs heap
//!   4. scheduler — needs heap
//!   5. syscall   — needs scheduler
//!
//! Copyright (c) 2026 TrueSystems LLC. All rights reserved.

#![no_std]
#![no_main]

extern crate alloc;

// --- Subsystem modules -----------------------------------------------------
// `boot` is assembly + linker script only (no mod.rs) and is linked via
// src/boot/boot.asm + src/boot/linker.ld, so it is not declared as a module.
mod fs;
mod memory;
mod scheduler;
mod sentinel;
mod syscall;
mod safemode; 

/// Kernel entry point.
///
/// AGREED CONTRACT: boot hands control to `kernel_main` with the UEFI memory
/// map pointer as the first argument (rdi, System V AMD64 ABI). `memory::init`
/// consumes that same pointer.
///
/// COMPATIBILITY NOTE (see integration report): the committed boot subsystem
/// does not yet honor this contract — `src/boot/boot.asm` forwards to a Rust
/// symbol named `bootloader_main` using `extern "efiapi"` (Microsoft x64,
/// RCX/RDX = ImageHandle/SystemTable), and `src/boot/linker.ld` declares
/// `ENTRY(_kernel_start)`. Neither `kernel_main` nor a System-V memory-map
/// handoff exists on the boot side yet. This function is written to the agreed
/// contract; the boot subsystem must be aligned to it before the final link.
#[no_mangle]
pub extern "C" fn kernel_main(memory_map: *const ()) -> ! {
    // Banner first — emitted before any subsystem so the serial log shows the
    // kernel reached its entry point even if a later init faults.
    // SAFETY: COM1 (0x3F8) is a fixed legacy port; the kernel runs at CPL 0, so
    // the privileged `out` instruction is permitted. `serial_write` only issues
    // port writes — it touches no memory and no shared state.
    unsafe {
        serial_write("Golem Linux gkern v0.1.0\n");
    }

    // 1. SENTINEL FIRST. The invisibility gate must be live before any other
    //    subsystem initializes.
    sentinel::init();
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("  sentinel: initialized\n");
    }

    // 2. MEMORY. Bring the heap up before anything allocates. The boot-provided
    //    memory-map pointer is forwarded verbatim.
    if memory::init(memory_map).is_err() {
        // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
        unsafe {
            serial_write("  memory: FAILED\n");
        }
        halt();
    }
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("  memory: initialized\n");
    }

    // 3. MIGRATION. Apply any Sentinel config changes written in Safe Mode.
    //    Must run after memory (allocates for SHA-256 + audit append).
    //    Failure halts — an unresolved Sentinel state is not recoverable.
    if let Err(e) = sentinel::migration::run() {
        unsafe {
            serial_write("  migration: FAILED — ");
            serial_write(e.reason());
            serial_write("\n");
        }
        halt();
    }
    unsafe {
        serial_write("  migration: ok\n");
    }

    // 4. FILESYSTEM. Needs the heap. init() mounts ramfs at "/".
    fs::init();
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("  fs: initialized\n");
    }

    // 5. SCHEDULER. Needs the heap.
    if scheduler::init().is_err() {
        // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
        unsafe {
            serial_write("  scheduler: FAILED\n");
        }
        halt();
    }
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("  scheduler: initialized\n");
    }

    // 6. SYSCALL. Needs the scheduler. init() writes the LSTAR MSR.
    // SAFETY: CPL 0, long mode active, scheduler initialized — the documented
    // preconditions for the LSTAR MSR write are satisfied.
    unsafe {
        syscall::init();
    }
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("  syscall: initialized\n");
    }

    // All subsystems up — announce readiness, then enter the kernel idle loop.
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        serial_write("Golem Linux ready\n");
    }
    halt();
}

/// Write a string to the COM1 serial port (0x3F8), one byte at a time.
///
/// This is the kernel's debug-output primitive: QEMU forwards COM1 to stdio
/// (see `scripts/run_qemu.sh`), so anything written here appears in the boot
/// log. It performs a raw `out dx, al` per byte with no FIFO/line-status
/// handshake — adequate for low-volume early-boot banners under emulation.
///
/// SAFETY: callers must guarantee CPL 0 (privileged `out`). The function reads
/// no memory the caller doesn't own (`s` is a normal `&str`) and writes only to
/// the I/O port, so it is sound for any well-formed string at ring 0.
unsafe fn serial_write(s: &str) {
    for byte in s.bytes() {
        // SAFETY: `out dx, al` writes one byte to the COM1 data register. It is
        // privileged but legal at CPL 0; it touches no memory and preserves
        // flags, hence the options below.
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Park the current CPU forever.
fn halt() -> ! {
    loop {
        // SAFETY: `hlt` is privileged (CPL 0); the kernel always runs in ring 0.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Panic handler — mandatory under `no_std`. No subsystem defines one, so the
/// crate root provides it. With `panic = "abort"` there is nothing to unwind;
/// a panicked kernel simply halts.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    halt();
}
