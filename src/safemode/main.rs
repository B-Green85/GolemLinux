//! gkern-safemode — Golem Linux Safe Mode kernel crate root (Phase 5, Agent 1).
//!
//! Safe Mode is a separate, minimal, agent-free recovery environment. Per
//! `GOLEM_RUNTIME_ARCHITECTURE.md` § "Safe Mode" it is the *only* context in
//! which Sentinel may be (re)configured, and per its stated properties it runs
//! with:
//!
//!   * No LLM processes. No agents. No exceptions.
//!   * No network services beyond the configuration task.
//!   * All other OS services unavailable.
//!
//! This crate root is therefore deliberately spartan. It is a **separate binary
//! from `gkern`** (`[[bin]] name = "gkern-safemode"` in `Cargo.toml`) that boots
//! via the *same* UEFI bootloader and the *same* entry convention: the loader
//! parses the kernel ELF (`src/boot/linker.ld`, `ENTRY(kernel_main)`), installs
//! the per-section page tables, and jumps — System V AMD64 — to `kernel_main`
//! with `RDI` = the boot→memory handoff pointer (`memory::BootHandoff`). We do
//! NOT introduce a new entry symbol; `kernel_main` here mirrors the one in
//! `src/main.rs` exactly so the existing bootloader needs no changes.
//!
//! What Safe Mode brings up, in order, and nothing else:
//!   1. memory  — heap/paging, so the config interface has a working allocator
//!                and the migration block lives in mapped RW memory.
//!   2. serial  — a real (read+write) 16550 UART on COM1 for the operator.
//!   3. Sentinel config interface — the interactive terminal in [`safemode`].
//!
//! What Safe Mode deliberately does NOT bring up: the scheduler, the syscall
//! interface, the filesystem, the Sentinel IPC / agent-registration channel.
//! There is no path in this binary by which an LLM or agent process can be
//! registered — that is an architectural property of the environment, not a
//! runtime check that could be toggled.
//!
//! Copyright (c) 2026 Brandon Green. Licensed under the Apache 2.0 License.

#![no_std]
#![no_main]

// The kernel heap (`memory::heap`) registers the `#[global_allocator]`, which
// is what gives the `alloc` crate a backing allocator. We keep `alloc` linked
// for parity with `gkern` and so the heap brought up by `memory::init` is a
// genuinely usable one; Safe Mode itself stays allocation-light and does not
// reference `alloc` directly, hence the allow (the link is the point).
#[allow(unused_extern_crates)]
extern crate alloc;

// --- Subsystems -------------------------------------------------------------
//
// `memory` is the EXACT same source the main kernel uses — pulled in by path,
// not copied, so Safe Mode and `gkern` can never drift in how they read the
// boot handoff or arm the heap. We are only permitted to *create* files under
// `src/safemode/`, and `#[path]` includes a sibling module without modifying
// it, which is precisely the intent here.
#[path = "../memory/mod.rs"]
mod memory;

// The Safe Mode body: serial driver, the `SentinelConfig` model, the migration
// block, and the interactive configuration terminal. Lives in `mod.rs` next to
// this file; named explicitly because a binary crate root does not adopt a
// sibling `mod.rs` as a directory module automatically.
#[path = "mod.rs"]
mod safemode;

/// Safe Mode kernel entry point.
///
/// SAME CONTRACT AS `gkern` (see `src/boot/linker.ld` and `src/main.rs`): the
/// bootloader jumps here under the System V AMD64 ABI with the UEFI-derived
/// memory-map / boot-handoff pointer in `RDI` (the first `extern "C"` argument).
/// `memory::init` consumes that same pointer. We do not define a new symbol and
/// do not change the handoff convention — `gkern-safemode` is reached by the
/// identical loader path as `gkern`.
#[no_mangle]
pub extern "C" fn kernel_main(memory_map: *const ()) -> ! {
    // Banner first, before any subsystem, so the serial log proves we reached
    // the Safe Mode entry even if a later init faults.
    //
    // SAFETY: COM1 (0x3F8) is a fixed legacy I/O port and the kernel runs at
    // CPL 0, so the privileged `out` issued by `early_serial_write` is legal.
    // The helper only performs port writes — no memory, no shared state.
    unsafe {
        safemode::early_serial_write("Golem Linux gkern-safemode v0.1.0 (SAFE MODE)\n");
    }

    // 1. MEMORY. Bring the heap up so the config interface and migration block
    //    sit in mapped RW memory. The boot-provided pointer is forwarded
    //    verbatim, exactly as `gkern` does.
    if memory::init(memory_map).is_err() {
        // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
        unsafe {
            safemode::early_serial_write("  memory: FAILED\n");
        }
        halt();
    }
    // SAFETY: see banner — CPL 0, fixed COM1 port, port-write-only helper.
    unsafe {
        safemode::early_serial_write("  memory: initialized\n");
    }

    // 2 + 3. SERIAL + Sentinel config interface. `safemode::run` initialises the
    //        full-duplex UART and enters the interactive configuration terminal.
    //        It never returns: Safe Mode's whole life is that terminal.
    //
    // NOTE what is *absent*: no `scheduler::init`, no `syscall::init`, no
    // `fs::init`, no `sentinel::init` (the agent-registration IPC channel).
    // Safe Mode starts no scheduler, exposes no syscall surface, mounts no
    // filesystem, and registers no agents.
    safemode::run()
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

/// Panic handler — mandatory under `no_std`. With `panic = "abort"` there is
/// nothing to unwind; a panicked Safe Mode kernel simply halts.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    halt();
}
