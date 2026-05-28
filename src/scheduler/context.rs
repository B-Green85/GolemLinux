//! x86_64 context switch.
//!
//! `context_switch(prev_rsp_save, next_rsp)` saves the callee-saved register
//! set plus RSP into `*prev_rsp_save` and restores them from a previously
//! saved frame at `next_rsp`. It is the *only* place in this module where
//! we touch CPU state directly.
//!
//! Volatile (caller-saved) registers are intentionally not saved here.
//! The compiler spills them across an `extern "C"` call automatically;
//! preemption from a timer IRQ saves them in the IRQ entry stub (which
//! lives in the kernel's interrupt module, not here) before invoking the
//! scheduler.

use core::arch::global_asm;

unsafe extern "C" {
    /// Save the current task's callee-saved register state to `*prev_rsp_save`
    /// and resume execution from the frame at `next_rsp`.
    ///
    /// # Calling convention (SysV AMD64)
    ///
    /// * `rdi` = `prev_rsp_save`: a `*mut usize` slot to write the outgoing
    ///   RSP into. Receives the address of the suspended frame on this stack.
    /// * `rsi` = `next_rsp`: a `usize` previously written into another task's
    ///   save slot, OR the value returned by `process::init_kernel_stack` for
    ///   a process being run for the first time.
    ///
    /// On the first `context_switch` *into* a freshly-spawned process, the
    /// `ret` at the end of this routine transfers control to
    /// `process_entry_trampoline`, which re-enables interrupts and jumps to
    /// the process's `entry` (held in `r12` by `init_kernel_stack`).
    ///
    /// # Safety
    ///
    /// * `prev_rsp_save` must point to writable storage owned by the outgoing
    ///   task (typically a field on its PCB, or a discarded local if the
    ///   outgoing task is being destroyed and will never resume).
    /// * `next_rsp` must be the saved RSP of a task whose stack and PCB are
    ///   still alive — dropping a task's stack while it is still referenced
    ///   here is a use-after-free.
    /// * The scheduler lock (or `IrqGuard`) must be held across the call so
    ///   that no other path observes the half-switched state.
    pub fn context_switch(prev_rsp_save: *mut usize, next_rsp: usize);

    /// First-instruction landing pad for a freshly-spawned process.
    ///
    /// `init_kernel_stack` synthesizes a stack such that `context_switch`'s
    /// final `ret` jumps here, with the process's real entry pointer in `r12`.
    /// The trampoline re-enables interrupts (matching the `cli` performed by
    /// the spawning task's `IrqGuard`, which never gets a chance to run its
    /// `Drop` on a brand-new process) and tail-jumps to `entry`.
    ///
    /// # Safety
    ///
    /// Not callable from Rust — this symbol exists only as a jump target for
    /// `context_switch`. Calling it directly would `sti` and then jump to
    /// whatever happens to be in `r12`.
    pub fn process_entry_trampoline() -> !;
}

// Assembly is AT&T syntax (the GNU default), to match the rest of gkern's
// boot/IRQ stubs. If the kernel later standardizes on Intel syntax we can
// flip `options(att_syntax)` and rewrite the directives.
global_asm!(
    r#"
    .text

    .global context_switch
    .type   context_switch, @function
context_switch:
    # Save callee-saved registers onto the outgoing task's stack.
    # Order matters: it must mirror init_kernel_stack's synthesized frame
    # and the pop sequence below.
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    # Stash the outgoing RSP into *prev_rsp_save (rdi).
    movq %rsp, (%rdi)

    # Switch onto the incoming task's stack.
    movq %rsi, %rsp

    # Restore callee-saved registers from the incoming frame. Reverse order.
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp

    # `ret` pops the return address: either the suspended task's resume PC
    # (saved by the matching call) or `process_entry_trampoline` for a
    # process being run for the first time.
    ret
    .size context_switch, . - context_switch


    .global process_entry_trampoline
    .type   process_entry_trampoline, @function
process_entry_trampoline:
    # Re-enable interrupts. The spawning task did `cli` via IrqGuard, and
    # its Drop (which would do `sti`) never runs on a brand-new process.
    sti

    # Entry pointer was placed into r12 by init_kernel_stack.
    # Entry is typed `extern "C" fn() -> !`, so a plain jmp is correct;
    # ABI stack alignment (RSP == 8 mod 16) was set up by init_kernel_stack.
    jmpq *%r12

    # If a buggy entry returns despite its `-> !` signature, we fall through.
    # Halt rather than execute whatever happens to be on the stack next.
1:  cli
    hlt
    jmp 1b
    .size process_entry_trampoline, . - process_entry_trampoline
    "#,
    options(att_syntax)
);
