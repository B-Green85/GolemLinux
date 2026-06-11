//! Hardware drivers for the Golem kernel (gkern).
//!
//! Phase 6 brings up the first piece of hardware awareness: CPU identification
//! via [`cpuid`]. The kernel calls [`init`] once at boot — **after** the memory
//! subsystem is up and **before** the scheduler starts — to validate it is
//! running on the expected (Intel) hardware and to record that hardware in the
//! serial boot log.
//!
//! # Module layout
//!
//! * [`cpuid`] — read and decode the `CPUID` instruction: vendor string,
//!   family/model/stepping, and the SSE/SSE2/AVX feature flags.
//!
//! # Dependency assumptions
//!
//! This module is consumed by the kernel crate (`src/main.rs`, owned by the
//! integration agent), which is responsible for calling [`init`] in the
//! documented init order. The driver needs nothing from earlier subsystems —
//! `CPUID` is read-only and always legal in long mode — but it is sequenced
//! after memory init so its serial output lands in the expected place in the
//! boot log. It does **not** allocate, so it would also be safe before the
//! heap; the ordering is purely about log readability.
//!
//! # `no_std`
//!
//! This subsystem is `no_std`: it links only `core`. The authoritative
//! `#![no_std]` lives at the crate root (`src/main.rs`); the inner attribute
//! below mirrors the convention the sibling subsystems use (see
//! `scheduler/mod.rs`) so the module stays std-free under `cargo test`, where
//! `std` is linked for the test harness — hence the `not(test)` guard. As an
//! inner attribute in a non-root module it is a no-op for the real kernel
//! build; the guarantee here is enforced by the imports, not by this line.
#![cfg_attr(not(test), no_std)]

pub mod cpuid;

use core::fmt::{self, Write};

/// COM1 serial data register. QEMU forwards COM1 to stdio (see
/// `scripts/run_qemu.sh`), so anything written here shows up in the boot log.
/// This mirrors the crate root's own debug primitive (`serial_write` in
/// `src/main.rs`), which is private to that module; the driver carries its own
/// minimal writer so it has no cross-module dependency for boot output.
const COM1: u16 = 0x3F8;

/// Write one byte to COM1.
///
/// SAFETY: the caller must be at CPL 0 (the privileged `out` instruction). The
/// kernel always runs in ring 0. The write touches no memory and preserves
/// flags. On non-x86 build hosts this is a no-op so the module still compiles
/// for `cargo test`; the kernel only ever runs the real path on `x86_64`.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn outb_com1(byte: u8) {
    // SAFETY: `out dx, al` writes one byte to the COM1 data register — a
    // privileged but legal operation at CPL 0 that touches no memory.
    core::arch::asm!(
        "out dx, al",
        in("dx") COM1,
        in("al") byte,
        options(nomem, nostack, preserves_flags),
    );
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
unsafe fn outb_com1(_byte: u8) {}

/// A zero-sized [`core::fmt::Write`] sink that emits to COM1, so the driver can
/// format its boot lines with the ordinary `write!` machinery instead of
/// hand-rolling number-to-string conversions.
struct SerialOut;

impl Write for SerialOut {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // SAFETY: the kernel runs at CPL 0, so the port write is legal; see
            // `outb_com1`. This is the only place the driver emits output.
            unsafe { outb_com1(byte) };
        }
        Ok(())
    }
}

/// Initialise the hardware-detection drivers.
///
/// Detects the CPU via [`cpuid::detect`] and writes a multi-line summary to the
/// serial boot log: the vendor string (with an explicit check against the
/// expected Intel vendor), the family/model/stepping, and the SSE/SSE2/AVX
/// feature flags. `CPUID` is read-only and always legal in long mode, so this
/// is sound to call at any point after boot reaches Rust; the kernel sequences
/// it after memory init and before the scheduler.
///
/// Returns the detected [`cpuid::CpuInfo`] so a caller could gate later
/// decisions on it; the boot path is free to ignore the value.
pub fn init() -> cpuid::CpuInfo {
    let info = cpuid::detect();

    // `writeln!`/`write!` into a serial sink cannot fail (our `write_str`
    // always returns `Ok`), so the `Result` is intentionally discarded.
    let mut out = SerialOut;
    let _ = writeln!(out, "  cpuid: detecting CPU");
    let _ = writeln!(out, "{}", info);

    info
}
