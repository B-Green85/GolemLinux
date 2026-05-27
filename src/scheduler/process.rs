//! Process Control Block (PCB) for the Golem kernel scheduler.
//!
//! A `Process` here is a kernel-schedulable task. It owns its kernel stack
//! and carries enough state for the round-robin scheduler in `scheduler.rs`
//! to suspend it via `context_switch` and later resume it.
//!
//! Golem's core principle — *every process has an owner* — is enforced at the
//! PCB level: an `OwnerId` is a mandatory field on construction. Code paths
//! that try to spawn without an owner cannot type-check.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

/// Process identifier. Kernel-unique, monotonically allocated.
pub type Pid = u64;

/// Identifier of the accountable owner of a process.
///
/// This is intentionally opaque — the auth/sentinel layer is responsible for
/// resolving an `OwnerId` to a human, agent, or service principal. The
/// scheduler treats it as bookkeeping; Sentinel treats it as accountability.
pub type OwnerId = u64;

/// PID 0 is reserved for the bootstrap context that exists before any real
/// process is spawned (the kernel's initial execution thread).
pub const PID_BOOTSTRAP: Pid = 0;

/// PID 1 is reserved for the idle process — always exists, always runnable,
/// runs `hlt` in a loop when nothing else has work to do.
pub const PID_IDLE: Pid = 1;

static NEXT_PID: AtomicU64 = AtomicU64::new(2);

/// Allocate a fresh PID. Monotonic, never reused for the lifetime of the kernel.
///
/// PID exhaustion (2^64 spawns) is treated as unreachable; if it ever happens
/// the kernel should panic rather than wrap.
pub fn alloc_pid() -> Pid {
    let p = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    assert!(p != Pid::MAX, "kernel PID space exhausted");
    p
}

/// Lifecycle states a process may occupy.
///
/// The scheduler only schedules `Ready` and `Running`. `Blocked` processes are
/// parked in synchronization primitives (waitqueues) owned elsewhere in the
/// kernel; the scheduler tracks them in a holding list only so they don't get
/// dropped. `Zombie` processes are headed for reaping on the next schedule
/// boundary — see `scheduler::schedule` for the safe-drop protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Zombie,
}

/// Entry-point signature for a freshly-spawned kernel task.
///
/// The `!` return type encodes the rule that a process must terminate via
/// `scheduler::exit_current()` rather than falling off the end of its entry
/// function. Falling through would land in `process_entry_trampoline`'s
/// halt-loop guard.
pub type EntryFn = extern "C" fn() -> !;

/// Process Control Block — the kernel's full record of a single task.
///
/// Layout note: `kernel_rsp` is the *first* field deliberately. The context
/// switch routine in `context.rs` is handed `&mut self.kernel_rsp` directly,
/// and keeping it at a known offset makes the assembly side trivial to
/// reason about even if other fields are reordered later.
#[repr(C)]
pub struct Process {
    /// Saved kernel-stack pointer. Updated by `context_switch` whenever this
    /// process is suspended; consumed by `context_switch` when resuming it.
    pub kernel_rsp: usize,

    pub pid: Pid,
    pub owner: OwnerId,
    pub name: String,
    pub state: ProcessState,

    /// Backing storage for the kernel stack. The PCB owns it so the stack is
    /// freed exactly when the PCB is dropped — see `scheduler::schedule` for
    /// why that drop is deferred past `exit_current`.
    pub kernel_stack: Box<[u8]>,

    /// Ticks remaining in the current time slice. Decremented by `tick()`;
    /// reaching zero triggers preemption.
    pub time_slice_remaining: u32,
    /// Default ticks allocated per slice. Replenished on requeue.
    pub default_time_slice: u32,
}

impl Process {
    /// Construct a freshly-spawned process ready to be scheduled.
    ///
    /// The `kernel_stack` buffer must be large enough to fit at least the
    /// synthesized launch frame (seven 8-byte words plus alignment padding)
    /// plus whatever the entry function actually needs. The scheduler does
    /// not check this — undersize stacks will silently corrupt memory.
    pub fn new(
        name: String,
        owner: OwnerId,
        entry: EntryFn,
        kernel_stack: Box<[u8]>,
        time_slice: u32,
    ) -> Self {
        Self::new_with_pid(alloc_pid(), name, owner, entry, kernel_stack, time_slice)
    }

    /// Variant that lets callers supply a reserved PID — used for the idle
    /// process (PID_IDLE) which must exist before normal PID allocation begins.
    pub fn new_with_pid(
        pid: Pid,
        name: String,
        owner: OwnerId,
        entry: EntryFn,
        mut kernel_stack: Box<[u8]>,
        time_slice: u32,
    ) -> Self {
        let stack_base = kernel_stack.as_mut_ptr();
        let stack_size = kernel_stack.len();

        // SAFETY: `kernel_stack` is a valid, exclusively-owned heap allocation
        // of `stack_size` bytes. `init_kernel_stack` writes only within that
        // range (from the 16-byte-aligned top downward by at most 7 * 8 + 8
        // bytes). The returned `usize` is an address inside the same allocation
        // and is stored in `kernel_rsp` for later use by `context_switch`.
        let kernel_rsp = unsafe { init_kernel_stack(stack_base, stack_size, entry) };

        Self {
            kernel_rsp,
            pid,
            owner,
            name,
            state: ProcessState::Ready,
            kernel_stack,
            time_slice_remaining: time_slice,
            default_time_slice: time_slice,
        }
    }

    /// Construct a PCB for the *currently running* kernel context — the one
    /// that's executing right now during bootstrap, before any context switch
    /// has happened. `kernel_rsp` and `kernel_stack` are placeholders; the
    /// first switch *off* this process will write the real RSP into the slot.
    ///
    /// We hand this an empty `Box<[u8]>` because the bootstrap stack was
    /// allocated by the bootloader / boot agent, not by us, and we must not
    /// try to free it on drop.
    pub fn bootstrap(owner: OwnerId) -> Self {
        Self {
            kernel_rsp: 0,
            pid: PID_BOOTSTRAP,
            owner,
            name: String::from("<bootstrap>"),
            state: ProcessState::Running,
            kernel_stack: Box::new([]),
            time_slice_remaining: 0,
            default_time_slice: 0,
        }
    }
}

/// Set up a freshly-allocated kernel stack so that the first `context_switch`
/// into this process lands in `process_entry_trampoline`, which then calls
/// `entry`.
///
/// Layout produced (high → low addresses), matching the pop sequence in
/// `context.rs::context_switch`:
///
/// ```text
///   top_aligned          [end of stack, exclusive]
///   top_aligned - 8      alignment pad   (so the slot below is 16-aligned)
///   top_aligned - 16     trampoline addr (popped by `ret`)
///   top_aligned - 24     rbp = 0
///   top_aligned - 32     rbx = 0
///   top_aligned - 40     r12 = entry     <-- trampoline reads entry from r12
///   top_aligned - 48     r13 = 0
///   top_aligned - 56     r14 = 0
///   top_aligned - 64     r15 = 0         <-- initial RSP
/// ```
///
/// # Safety
///
/// `stack_base` must be a valid, writable pointer to at least `stack_size`
/// bytes that the caller has agreed to dedicate as this process's kernel
/// stack. The function writes to the top 64 bytes (plus up to 15 bytes of
/// alignment slop); `stack_size` must be at least 80 bytes for that to fit.
unsafe fn init_kernel_stack(
    stack_base: *mut u8,
    stack_size: usize,
    entry: EntryFn,
) -> usize {
    // We need at minimum 7 qwords (56 B) + up to 15 B of alignment slop.
    // Demand 80 to leave headroom and to catch absurdly small stacks.
    debug_assert!(stack_size >= 80, "kernel stack too small for launch frame");

    // SAFETY: caller guarantees [stack_base, stack_base + stack_size) is a
    // valid, writable allocation. We only access the highest ~64 bytes.
    let top = (stack_base as usize).wrapping_add(stack_size);
    let top_aligned = top & !0xF;
    let mut sp = top_aligned as *mut u64;

    // Alignment pad: ensures the return-address slot below sits at a 16-byte
    // aligned address. After the synthesized `ret` pops it, RSP becomes
    // `8 mod 16`, which is the alignment the SysV AMD64 ABI requires at
    // function entry.
    sp = sp.sub(1);
    // SAFETY: still inside the allocation (top is the exclusive end, and we
    // verified stack_size >= 80; `sp` is at offset top_aligned-8 from base).
    sp.write(0);

    // Return target for context_switch's final `ret`.
    sp = sp.sub(1);
    // SAFETY: same allocation, one more qword down.
    sp.write(super::context::process_entry_trampoline as usize as u64);

    // Synthesized callee-saved register frame, in PUSH order (rbp first,
    // r15 last). context_switch pops them in the reverse order, so r15
    // ends up at the lowest address and is the first thing popped.
    let saved: [u64; 6] = [
        0,                          // rbp
        0,                          // rbx
        entry as usize as u64,      // r12 — trampoline reads entry from here
        0,                          // r13
        0,                          // r14
        0,                          // r15
    ];
    for &v in saved.iter() {
        sp = sp.sub(1);
        // SAFETY: each iteration moves sp down one qword; six iterations
        // stays within the top 64 bytes of the allocation, which we
        // validated above.
        sp.write(v);
    }

    sp as usize
}
