# gkern Scheduler

**Agent 3 of 6 — Process Scheduler**

Round-robin, preemptive, single-CPU process scheduler for the Golem kernel
(gkern). x86_64 only. Rust with one Assembly file for the context switch.

```
src/scheduler/
├── mod.rs        — public API surface
├── process.rs    — Process Control Block, PID allocation, stack synthesis
├── scheduler.rs  — round-robin core, IRQ-safe critical section, idle/zombie handling
├── context.rs    — extern decls + global_asm! for the context switch
└── README.md     — this file
```

---

## Scope

What this module does:

- Tracks all kernel-schedulable tasks (kernel threads and the kernel half of
  user processes — userspace transitions are another agent's concern).
- Picks the next task to run via strict FIFO round-robin.
- Preempts the running task when its time slice expires.
- Switches CPU register state between tasks.
- Reaps exited tasks safely.
- Tracks blocked tasks so the kernel's synchronization primitives can park /
  wake them.

What this module deliberately does *not* do:

- **Memory management.** Kernel stacks are passed in as `Box<[u8]>` by the
  caller. Allocation is the memory agent's job.
- **Userspace transitions.** `iretq`, syscall returns, ring-3 entry — that's
  the syscall agent's surface.
- **SMP.** Single CPU. See "Single-CPU assumption" below.
- **Priorities.** Round-robin only in v1. Priority lanes can be added by
  splitting `ready` into `VecDeque`s per priority and picking the highest
  non-empty one in `schedule()`.
- **Sleep / timer-driven wake.** Blocked tasks need an explicit `unblock(pid)`
  from whichever subsystem put them to sleep.

---

## Decisions

### Round-robin, FIFO, fixed time slice

`DEFAULT_TIME_SLICE = 10` ticks. At a planned 1 kHz timer rate this is a
10 ms quantum — a reasonable middle ground between context-switch overhead
and interactive responsiveness. Each Process carries its own
`default_time_slice`, so per-process tuning is possible without touching the
scheduler.

The ready queue is a single `VecDeque<Box<Process>>`. Pop from the front,
push to the back. No priority weighting, no fairness accounting beyond
"everyone gets the same slice."

**Why round-robin first.** It is the simplest preemptive policy that has any
hope of being correct, and Golem's accountability model (Sentinel watches
agent behavior at the process boundary) does not care about scheduler
sophistication — it cares about deterministic, observable scheduling
decisions. We can layer priorities or CFS-style fairness on later without
touching the public API.

### Context switch saves callee-saved registers only

`context_switch(prev_save: *mut usize, next_rsp: usize)` saves `rbp, rbx,
r12-r15` plus `rsp`. It does **not** save the volatile (caller-saved) set
(`rax, rcx, rdx, rsi, rdi, r8-r11, rflags`).

**Why.** Two reasons:

1. **Cooperative switches** (`yield_now`, `block_current`, `exit_current`)
   go through Rust's `extern "C"` calling convention to reach
   `context_switch`. The compiler spills any live volatile registers across
   the call automatically — re-saving them here would be redundant work on
   every yield.

2. **Preemptive switches** come from the timer IRQ. The IRQ entry stub
   (owned by the interrupt module, not this one) must save the full volatile
   register set onto the kernel stack before it can call any C-ABI function,
   `tick()` included. By the time `context_switch` runs, those registers
   are already on the stack as part of the IRQ trap frame; switching to a
   different stack carries the trap frame along, and the eventual
   return-from-IRQ restores them.

This is the same split Linux and xv6 use. It's the right one.

### First-run trampoline (`process_entry_trampoline`)

A freshly-spawned process has never executed before; its "saved register
state" is fabricated by `init_kernel_stack` in `process.rs`. The synthesized
frame places the entry-function pointer in `r12` and the trampoline address
in the return slot, so `context_switch`'s final `ret` jumps to the
trampoline.

The trampoline does two things:

1. `sti` — re-enables interrupts. The spawning task held an `IrqGuard`
   (which `cli`s on construction and `sti`s on drop), but its `Drop` never
   runs on a brand-new process because that stack frame is on a stack we
   abandoned. The trampoline restores the invariant manually.
2. `jmpq *%r12` — tail-jumps to entry. Entry is typed `extern "C" fn() -> !`,
   so a `jmp` is sound; ABI stack alignment was set up by
   `init_kernel_stack`.

If a buggy entry function ever returns despite its `-> !` signature, the
trampoline falls through to a `cli; hlt; jmp .` loop rather than executing
whatever happens to be next in memory.

### IRQ-disable critical section, not a spinlock

`GlobalScheduler::with` wraps each scheduler operation in an `IrqGuard`
that `cli`s on construction and restores RFLAGS on drop. On a single CPU,
this is sufficient mutual exclusion: no other code path can be running on
this CPU until interrupts are re-enabled. We don't need a spinlock.

**Why no `spin::Mutex`.** A spin lock buys us nothing here that `cli` doesn't
already give us, and it would add a dependency we'd then have to maintain
through `no_std`. When SMP comes, we'll need both — `cli` for local
non-reentrance, a spinlock for cross-CPU exclusion — but we don't need it
yet, so we don't pay for it.

**Why RFLAGS save/restore instead of plain `cli/sti`.** If a caller already
had interrupts disabled (e.g., the timer IRQ handler invoking `tick`), a
naive `sti` on drop would re-enable interrupts in a context that wasn't
supposed to have them. `pushfq` / `popfq` makes the guard transparent to
nesting.

### Zombie reaping deferred to next schedule

`exit_current` cannot free its own kernel stack — it is still running on it.
The current task is moved to a `defunct: Vec<Box<Process>>` slot and
`schedule()` is invoked. The next entry to `schedule()` (from some other
task, on some other stack) drains `defunct` at the very top, dropping the
Boxes and freeing their stacks.

This means a single exited task may sit in `defunct` until the next
scheduling event, which is fine — its stack is just heap memory at that
point and nothing references it.

### Bootstrap PCB

`init`/`init_with` create a `Process::bootstrap` with `kernel_rsp = 0` and an
empty `Box<[u8]>` stack. This represents the kernel's initial execution thread —
the one that called `init()`. We never need to *resume* this PCB (it has no
saved frame), but the first context switch off it needs somewhere to save
state. The empty `Box<[u8]>` ensures we don't accidentally try to free the
bootloader-provided stack on drop.

### Idle process

PID 1 is always the idle process. It is never enqueued in `ready`; it lives
in its own slot and runs only when `ready` is empty *and* the current task
is no longer Running. Its entry function is conventionally `loop { hlt }`.

The no-arg `init()` installs a built-in `idle_loop` (a `hlt`-spin) so the
integration layer needs nothing from the power-management module to bring the
scheduler up. A kernel that has its own idle/power-management entry can supply
it via `init_with(owner, idle_stack, idle_entry)` instead.

### Reserved PIDs

- `PID_BOOTSTRAP = 0` — the kernel's initial thread.
- `PID_IDLE = 1` — the idle process.
- Allocation for normal processes starts at 2 and is monotonic for the
  lifetime of the kernel. We panic rather than wrap on exhaustion (2^64
  spawns is unreachable on any plausible deployment).

### `Process` is `#[repr(C)]` with `kernel_rsp` first

The assembly side takes `&mut kernel_rsp` directly. Keeping it at offset 0
of the struct makes the layout robust to future field additions on the Rust
side — assembly only ever sees the one pointer.

### AT&T syntax in `global_asm!`

The boot and IRQ stubs in gkern use AT&T syntax (GNU `as` default).
Consistency beats personal preference; switching to Intel syntax later is a
mechanical rewrite if the rest of the kernel moves first.

---

## Integration (Phase 2)

This section records the Phase 2 work that readies the scheduler for the
integration agent (Agent 7) to wire into `kernel_main`.

### Entry point: `scheduler::init()`

`kernel_main` calls `scheduler::init()` with **no arguments**. It allocates the
idle task's kernel stack from the global heap and installs the built-in
`idle_loop` (a `hlt`-spin) with `KERNEL_OWNER` as the placeholder accountability
ID, then delegates to `init_with`. The full-control form `init_with(owner,
idle_stack, idle_entry)` remains available for callers (and tests) that want to
supply their own idle entry, stack, or owner.

### Initialization order — hard dependency

**Memory must be initialized before the scheduler.** `init()` allocates the
idle stack from the `#[global_allocator]`; calling it before `memory::init` has
registered the heap will fault. The required `kernel_main` order is:

```text
1. sentinel::init()          // governance — first
2. memory::init(memory_map)  // registers the global allocator
3. fs::init()
4. scheduler::init()         // ← allocates the idle stack from the heap
5. syscall::init()           // needs the scheduler
```

`scheduler::init()` must be called exactly once. After it returns, `spawn`,
`yield_now`, `tick`, etc. are live; before it, they are no-ops or will observe
an uninitialized scheduler.

### `no_std` compliance

Audited in Phase 2: **no `std::` path appears anywhere in `src/scheduler/`.**
Every import resolves through `core` or `alloc` (`Box`, `String`, `Vec`,
`VecDeque`, `UnsafeCell`, `core::arch::asm`/`global_asm`, `core::sync::atomic`).
`mod.rs` carries `#![cfg_attr(not(test), no_std)]` to match the sibling
subsystems' convention; the authoritative `#![no_std]` lives in the crate root
(`src/main.rs`). The crate root is also responsible for `extern crate alloc;`
(this module uses `Box`/`String`/`Vec`/`VecDeque`).

### Context-switch ABI verification

The x86_64 System V context switch in `context.rs` was reverified in Phase 2
and is **correct**:

- `context_switch` pushes all six callee-saved registers — `rbp, rbx, r12, r13,
  r14, r15` — saves the outgoing `rsp` into `*prev_rsp_save` (`rdi`), loads the
  incoming `rsp` (`rsi`), then pops the six registers in the exact reverse order
  and `ret`s. Save/restore are symmetric.
- The launch frame synthesized by `init_kernel_stack` (`process.rs`) matches the
  pop sequence: `ret` lands in `process_entry_trampoline`, with the real entry
  pointer pre-loaded into `r12`.
- Stack alignment is ABI-correct: after the trampoline `ret`, `rsp ≡ 8 (mod 16)`,
  exactly what SysV expects at a function entry reached via `call`.
- Volatile (caller-saved) registers are intentionally *not* saved here — see
  "Context switch saves callee-saved registers only" above.

No assembly changes were required.

---

## Single-CPU assumption

The whole module is built around the assumption that there is exactly one
CPU. Two concrete spots will break under SMP:

1. **`unsafe impl Sync for GlobalScheduler`** — currently justified by
   "interrupts disabled ⇒ no concurrent access on this CPU." Under SMP, two
   CPUs can both have interrupts disabled and both call into the scheduler
   simultaneously.
2. **Single `ready` queue** — fine for one CPU, a contention point for many.
   Per-CPU runqueues with work-stealing is the standard fix.

The SMP migration path:

- Wrap `Scheduler` in a real spinlock (still alongside the per-CPU
  `IrqGuard`, which we need to keep — IRQ handlers must not deadlock on a
  lock the interrupted code already holds).
- Promote `current`, `idle`, and `ready` to per-CPU.
- Add a CPU-id parameter to `tick()` and friends.

None of this changes the *public API* of `mod.rs`, which is the reason it's
deliberately small.

---

## Unsafe inventory

Every `unsafe` block in this module carries a `SAFETY:` comment. The full
list:

| File | What | Why it is safe |
|------|------|----------------|
| `process.rs::init_kernel_stack` | Writes 7 qwords + alignment pad near the top of a caller-supplied buffer | Caller guarantees the buffer is valid and ≥ 80 bytes; we only touch the top ~64 |
| `process.rs::Process::new_with_pid` | Calls `init_kernel_stack` | Buffer comes from a `Box<[u8]>` we own |
| `scheduler.rs::Scheduler::schedule` | Calls `context_switch` | `prev_rsp_ptr` is either a heap-stable PCB field or a discarded local; `next_rsp` came from `init_kernel_stack` or a prior save; interrupts are disabled by `IrqGuard` |
| `scheduler.rs::IrqGuard::new` | `pushfq; pop; cli` | Single-instruction state read; cannot race |
| `scheduler.rs::IrqGuard::drop` | `push; popfq` | Restores exactly the flags captured at construction |
| `scheduler.rs::GlobalScheduler::with` | `&mut *self.inner.get()` | Single CPU + IrqGuard ⇒ no other live borrow can exist |
| `context.rs` `global_asm!` | The whole context switch routine | Documented inline; ABI-compliant register save/restore |

---

## API summary

```rust
use gkern::scheduler;

// Once, at boot — the integration layer (kernel_main) calls the no-arg form
// AFTER memory::init has registered the global allocator:
scheduler::init()?;

// Or, if the kernel wants to supply its own idle entry / stack / owner:
scheduler::init_with(kernel_owner, idle_stack, idle_entry)?;

// Spawn a kernel task:
let pid = scheduler::spawn(name, owner, entry_fn, kernel_stack);

// Cooperative yield:
scheduler::yield_now();

// Park self until somebody calls unblock(self_pid):
scheduler::block_current();
scheduler::unblock(other_pid)?;

// Exit the calling task:
scheduler::exit_current(); // -> !

// From the timer IRQ stub:
scheduler::tick();
```

---

## What this does NOT do (to be explicit)

If the kernel needs any of the following, it is on the corresponding agent,
not this module:

- Allocating or freeing kernel stacks (memory agent).
- Wiring `tick()` into the IDT and timer programming (interrupt agent).
- Saving / restoring the volatile register set for IRQ-triggered preemption
  (interrupt agent).
- User-mode entry / exit via `iretq` or `sysret` (syscall agent).
- Sentinel handshake on spawn (sentinel agent reads `owner` from the PCB).
- The audit-trail entry for every spawn / exit (sentinel / audit agent).

---

*Copyright © 2026 Brandon Green. Licensed under the Apache 2.0 License.*
*Golem Linux is developed under the CDMAE methodology.*
