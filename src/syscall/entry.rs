//! x86_64 `SYSCALL`/`SYSRET` CPU entry path and MSR programming.
//!
//! This file owns the two things that live "below" `syscall_dispatch`: the
//! assembly trampoline the CPU jumps to on `SYSCALL`, and the per-CPU MSR
//! programming that arms it. Everything above `syscall_dispatch` is plain Rust
//! (see [`crate::syscall::dispatch`] and [`crate::syscall::handlers`]).
//!
//! # Why `global_asm!` and not a standalone `.asm` file
//!
//! Phase 1 wrote the entry trampoline as a standalone NASM file (`entry.asm`).
//! That cannot integrate: `gkern` has **no `build.rs` and no NASM step**, so a
//! loose `.asm` file is never assembled and `syscall_entry` would not exist in
//! the linked kernel — `IA32_LSTAR` would point at nothing. The rest of the
//! kernel solves the identical problem with `global_asm!` (see
//! `src/scheduler/context.rs`), so we follow that established convention here:
//! the trampoline is embedded via `global_asm!` (AT&T syntax) and assembled by
//! the integrated LLVM assembler into the kernel binary, and the MSRs are
//! programmed from Rust so the LSTAR handler address is taken *directly* from
//! the linked `syscall_entry` symbol — no hand-maintained literal, one source
//! of truth.
//!
//! # Linux x86_64 syscall ABI (matched bit-for-bit)
//!
//! ```text
//!   user register   purpose
//!   -------------   -----------------------------------------------------
//!   rax             syscall number          (also the return value on exit)
//!   rdi             arg0
//!   rsi             arg1
//!   rdx             arg2
//!   r10             arg3   (NOT rcx — rcx is clobbered by SYSCALL)
//!   r8              arg4
//!   r9              arg5
//!   rcx             clobbered: SYSCALL saves user RIP here
//!   r11             clobbered: SYSCALL saves user RFLAGS here
//! ```
//!
//! `SYSCALL` hardware behavior (Intel SDM Vol. 2B, AMD APM Vol. 3): loads RIP
//! from `IA32_LSTAR`; loads CS/SS from `IA32_STAR[63:48]`; saves user RIP→RCX,
//! user RFLAGS→R11; masks RFLAGS with `IA32_FMASK` (we clear IF to disable
//! interrupts on entry); does **not** switch stacks — we do that manually with
//! `swapgs` + `GS:[0]`. `SYSRET` reverses it: RIP←RCX, RFLAGS←R11.

use core::arch::{asm, global_asm};

unsafe extern "C" {
    /// CPU `SYSCALL` entry trampoline, defined in the `global_asm!` block below.
    ///
    /// Not callable from Rust — this symbol exists only so [`init_cpu`] can take
    /// its address and write it into `IA32_LSTAR`. The CPU jumps here directly
    /// on the `SYSCALL` instruction, in CPL 0, on the user stack, with `GS`
    /// still pointing at the user view.
    pub fn syscall_entry();
}

// ---------------------------------------------------------------------------
// MSR numbers (AMD APM Vol. 2 §3.1.7 / Intel SDM Vol. 4).
// ---------------------------------------------------------------------------
const MSR_EFER: u32 = 0xC000_0080; // Extended Feature Enable Register
const MSR_STAR: u32 = 0xC000_0081; // SYSCALL/SYSRET segment selectors
const MSR_LSTAR: u32 = 0xC000_0082; // SYSCALL target RIP (64-bit mode)
const MSR_FMASK: u32 = 0xC000_0084; // RFLAGS mask applied on SYSCALL entry

/// `EFER.SCE` — SYSCALL Enable. EFER.LME/LMA (long mode) are already set by the
/// boot agent before `kernel_main`; we only flip SCE here, via read-modify-write.
const EFER_SCE: u64 = 1 << 0;

/// `STAR` value: `STAR[47:32]` = kernel CS (0x08, SS = 0x10),
/// `STAR[63:48]` = user CS base (0x20 → SYSRET loads user CS64 at base+16 = 0x28
/// with RPL 3). Must match the GDT layout the memory/GDT agent installs.
const STAR_VALUE: u64 = 0x0023_0008_0000_0000;

/// `FMASK`: RFLAGS bits cleared on `SYSCALL` entry — IF|DF|TF|IOPL|NT|AC
/// (`0x47700`). Clearing IF disables interrupts the instant we enter the
/// kernel; DF normalizes string ops; TF blocks single-step traps from leaking
/// into kernel mode; IOPL/NT/AC are cleared defensively.
const FMASK_VALUE: u64 = 0x0004_7700;

/// Read a model-specific register. EDX:EAX ← MSR[ecx].
///
/// # Safety
///
/// `rdmsr` is privileged — it `#GP`s outside CPL 0. Caller must be in the
/// kernel (CPL 0). `msr` must be a readable MSR on this CPU.
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack, preserves_flags),
    );
    ((high as u64) << 32) | (low as u64)
}

/// Write a model-specific register. MSR[ecx] ← EDX:EAX.
///
/// # Safety
///
/// `wrmsr` is privileged — it `#GP`s outside CPL 0. Caller must be in the
/// kernel (CPL 0). Writing a reserved/invalid value also `#GP`s.
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        // Deliberately NOT `nomem`: arming LSTAR/STAR establishes the syscall
        // entry contract and must not be reordered ahead of the dispatch-table
        // writes that `syscall::init` performs first.
        options(nostack, preserves_flags),
    );
}

/// Program the four `SYSCALL`/`SYSRET` MSRs on the current CPU and arm `SYSCALL`.
///
/// Writes `IA32_LSTAR` with the linked address of [`syscall_entry`] so the CPU
/// has a valid handler the instant SCE is set. Wiring order within the
/// subsystem is fixed by [`crate::syscall::init`]: the dispatch table is
/// populated *before* this runs, so the very first `SYSCALL` sees a complete
/// table.
///
/// # Long mode and CPL 0 (privilege requirement)
///
/// `rdmsr`/`wrmsr` are **CPL 0-only** instructions; executing them at any other
/// privilege level raises `#GP(0)`. This routine must therefore run in kernel
/// (ring 0) context, *after* the CPU is already in 64-bit long mode — the boot
/// agent establishes long mode (EFER.LME/LMA, paging) before handing off to
/// `kernel_main`, so here we only set EFER.SCE. Calling this before long mode,
/// or from userspace, faults.
///
/// # Safety
///
/// Caller must be at CPL 0 in long mode, with the GDT loaded to match
/// [`STAR_VALUE`] and the per-CPU `GS` base configured (the trampoline reads
/// the kernel stack from `GS:[0]`). Call exactly once per CPU during bring-up
/// with interrupts disabled. Calling it twice is harmless but wasteful.
pub(crate) unsafe fn init_cpu() {
    // EFER.SCE = 1 — enable SYSCALL/SYSRET in long mode. Read-modify-write so we
    // preserve EFER.LME/LMA/NXE that the boot path already established.
    let efer = rdmsr(MSR_EFER);
    wrmsr(MSR_EFER, efer | EFER_SCE);

    // STAR — kernel/user segment selectors loaded by SYSCALL/SYSRET.
    wrmsr(MSR_STAR, STAR_VALUE);

    // LSTAR — the RIP the CPU loads on SYSCALL. This is the whole point of
    // init(): point it at our trampoline, taken straight from the linked symbol
    // so the address can never drift from the code it names.
    wrmsr(MSR_LSTAR, syscall_entry as usize as u64);

    // FMASK — RFLAGS bits cleared on entry (IF off ⇒ interrupts disabled).
    wrmsr(MSR_FMASK, FMASK_VALUE);
}

// ---------------------------------------------------------------------------
// syscall_entry — invoked directly by the CPU on the SYSCALL instruction.
//
// On entry: CPL 0, on the *user* stack, interrupts disabled (FMASK cleared IF),
// rcx = user RIP, r11 = user RFLAGS, GS still the user view.
//
// Per-CPU layout at GS:[...] (configured by the memory/CPU-init agent):
//   0 : kernel_rsp    — top of this CPU's kernel stack
//   8 : user_rsp_save — scratch slot to stash user RSP during entry
//
// AT&T syntax (`options(att_syntax)`), matching src/scheduler/context.rs.
// ---------------------------------------------------------------------------
global_asm!(
    r#"
    .text
    .global syscall_entry
    .type   syscall_entry, @function
syscall_entry:
    swapgs                          # GS -> kernel per-CPU area
    movq    %rsp, %gs:8             # stash user RSP in per-CPU scratch
    movq    %gs:0, %rsp             # load this CPU's kernel stack

    # Minimal trap frame. Order chosen so a future ptrace/signal path can read
    # it as a struct without re-shuffling.
    pushq   %gs:8                   # user RSP
    pushq   %r11                    # user RFLAGS
    pushq   %rcx                    # user RIP
    pushq   %rax                    # syscall nr (for restart)

    # Save the user registers the SysV ABI lets Rust clobber, so SYSRET can
    # restore the user's complete state.
    pushq   %rdi
    pushq   %rsi
    pushq   %rdx
    pushq   %r10
    pushq   %r8
    pushq   %r9
    pushq   %rbx
    pushq   %rbp
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15

    # Linux ABI -> SysV C ABI for the call into syscall_dispatch(nr, a0..a5):
    #   new rdi (nr)   <- rax
    #   new rsi (a0)   <- old rdi
    #   new rdx (a1)   <- old rsi
    #   new rcx (a2)   <- old rdx
    #   new r8  (a3)   <- r10
    #   new r9  (a4)   <- r8
    #   stack   (a5)   <- r9
    subq    $8, %rsp                # 16-byte align before the call
    pushq   %r9                     # a5 on stack (7th C arg)
    movq    %r8, %r9                # a4
    movq    %r10, %r8               # a3
    movq    %rdx, %rcx              # a2
    movq    %rsi, %rdx              # a1
    movq    %rdi, %rsi              # a0
    movq    %rax, %rdi              # nr

    call    syscall_dispatch

    addq    $16, %rsp               # drop a5 + alignment pad

    # rax holds the return value from Rust. Restore saved user registers.
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbp
    popq    %rbx
    popq    %r9
    popq    %r8
    popq    %r10
    popq    %rdx
    popq    %rsi
    popq    %rdi

    addq    $8, %rsp                # drop saved syscall-nr slot
    popq    %rcx                    # user RIP -> rcx for SYSRET
    popq    %r11                    # user RFLAGS -> r11 for SYSRET
    popq    %rsp                    # restore user RSP

    swapgs                          # GS -> user view
    sysretq                         # return to userspace (64-bit form)
    .size   syscall_entry, . - syscall_entry
    "#,
    options(att_syntax)
);
