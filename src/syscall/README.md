# `src/syscall/` — gkern syscall interface

> Owner: Agent 4 of 6 (syscall interface).
> Scope: this directory only. Other subsystems (memory, VFS, process manager, Sentinel, init) are owned by sibling agents.

This subsystem is the user/kernel boundary for Golem Linux. It is **bit-for-bit compatible with the Linux x86_64 syscall ABI** — same numbers, same register convention, same return semantics. A statically linked Linux binary that issues `SYSCALL` with the numbers listed below must observe identical entry/exit behavior on gkern as on Linux. Compatibility is not a goal to grow toward; it is a contract this directory enforces.

---

## File layout

| File           | Language          | Role                                                                       |
| -------------- | ----------------- | -------------------------------------------------------------------------- |
| `entry.rs`     | Rust + `global_asm!` | `SYSCALL` entry/exit trampoline (stack swap, ABI translation) + MSR setup |
| `dispatch.rs`  | Rust              | Dispatch table, syscall number constants, errno flattening                 |
| `handlers.rs`  | Rust              | Tier 1 handlers, boundary validation                                       |
| `mod.rs`       | Rust              | Public surface and bring-up entry point                                    |
| `README.md`    | Markdown          | This document                                                              |

> **Phase 2 change.** The entry path used to live in a standalone NASM file
> (`entry.asm`). It now lives in `entry.rs` as a `global_asm!` block plus Rust
> MSR programming. See [Phase 2 integration changes](#phase-2-integration-changes)
> for why.

The boundary between assembly and Rust is exactly one symbol: the `global_asm!` trampoline in `entry.rs` calls `syscall_dispatch` (defined in `dispatch.rs`) with the SysV C ABI. Everything below `syscall_dispatch` is high-level Rust; the trampoline above it (CPU state, segment selectors, stack swap) is assembly, and the MSR programming that arms it (`init_cpu`) is Rust driving `rdmsr`/`wrmsr`. This split is deliberate — Rust cannot soundly express SYSCALL *entry conditions* (interrupts off, wrong stack, half-restored segments), so the trampoline stays in assembly; but the MSR writes have a perfectly sound Rust model, so they move to Rust where the LSTAR address can be taken directly from the linked `syscall_entry` symbol. Each piece stays where it has a sound model.

---

## Linux x86_64 syscall ABI (the contract we match)

| Register | Purpose                                                              |
| -------- | -------------------------------------------------------------------- |
| `rax`    | Syscall number on entry; return value on exit                        |
| `rdi`    | arg0                                                                 |
| `rsi`    | arg1                                                                 |
| `rdx`    | arg2                                                                 |
| `r10`    | arg3 (**not** `rcx` — `rcx` is clobbered by the SYSCALL instruction) |
| `r8`     | arg4                                                                 |
| `r9`     | arg5                                                                 |
| `rcx`    | Clobbered: hardware writes user `RIP` here on SYSCALL                |
| `r11`    | Clobbered: hardware writes user `RFLAGS` here on SYSCALL             |

All other registers must be preserved across the boundary. Negative return values in the range `[-4095, -1]` are interpreted by userspace as `-errno`; non-negative values are successes. We never return a "negative success" — that would alias the errno space and break every libc on the planet.

---

## Tier 1 syscall numbers

These are the seven syscalls Agent 4 is responsible for. Numbers come straight from `arch/x86/entry/syscalls/syscall_64.tbl` in the Linux kernel tree and **must not** drift.

| Nr  | Name      | Signature                                                                  | Notes                                                                                          |
| --- | --------- | -------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| 0   | `read`    | `ssize_t read(int fd, void *buf, size_t count)`                            | Validates fd + user buffer, hands off to VFS.                                                  |
| 1   | `write`   | `ssize_t write(int fd, const void *buf, size_t count)`                     | Mirror of `read`.                                                                              |
| 2   | `open`    | `int open(const char *pathname, int flags, mode_t mode)`                   | Retained for ABI compat even though modern Linux prefers `openat(2)`.                          |
| 3   | `close`   | `int close(int fd)`                                                        | Returns 0 on success.                                                                          |
| 57  | `fork`    | `pid_t fork(void)`                                                         | Returns child PID to parent, 0 to child. COW semantics live in the process manager.            |
| 59  | `execve`  | `int execve(const char *pathname, char *const argv[], char *const envp[])` | On success, does not return — control resumes at the new image's entry point.                  |
| 60  | `exit`    | `void exit(int status)`                                                    | **Thread** exit (matches Linux). Full-process exit is `exit_group(2)` (nr 231) — a later tier. |

Unimplemented numbers (everything else in the 512-entry table) return `-ENOSYS`. This matches Linux for syscalls compiled out of the kernel and gives userspace a clean, documented failure mode rather than a fault.

---

## Why these decisions, specifically

This section documents every non-obvious choice. Future agents (and future-me) shouldn't have to re-derive the reasoning by reading hardware manuals.

### 1. Assembly for entry, Rust for handlers

Two reasons. **Soundness:** at the moment of `SYSCALL`, we are in CPL 0 with the *user* stack still in `RSP`, the wrong `GS` base, and `rcx`/`r11` holding state the CPU expects us to preserve for `SYSRET`. Rust's safety model assumes a working stack and standard ABI; we have neither. **Performance:** every cycle on the syscall path is paid for by every process on the system. Hand-written assembly lets us minimize the prologue/epilogue without fighting an optimizer.

### 2. `swapgs` + per-CPU `GS:[0]` stack pointer

`SYSCALL` does not switch stacks. Linux solves this with `SWAPGS`: a single instruction that swaps `IA32_KERNEL_GS_BASE` and `IA32_GS_BASE`. After `swapgs`, `GS:[0]` reads the kernel's per-CPU area, where the memory subsystem (sibling agent) has stashed the kernel stack pointer for this CPU. We use offset 0 for the kernel RSP and offset 8 for a scratch slot to stash user RSP during the prologue. This layout is hard-coded here and in the per-CPU init code; both must agree.

### 3. `IA32_FMASK` clears `IF`, `DF`, `TF` on entry

- `IF` cleared → interrupts off the instant we enter the kernel. We cannot service an interrupt on the user stack with a half-built frame.
- `DF` cleared → string ops (`rep movs` etc.) start in a known direction; SysV requires DF=0 on function entry anyway.
- `TF` cleared → a userspace process running under single-step debug cannot drag a `#DB` trap across the boundary.

The full mask (`0x47700`) also clears `IOPL`, `NT`, and `AC` defensively. None of these have any business being set in kernel mode.

### 4. Linux ABI → SysV C ABI shuffle in assembly

The Linux ABI uses `r10` for arg3; the SysV C ABI uses `rcx`. The C ABI also puts the *first* argument in `rdi`, but on syscall entry `rdi` already holds the user's arg0 and `rax` holds the syscall number. So `entry.asm` performs a rotation:

```
new rdi (a0 = nr)   <- rax
new rsi (a1 = arg0) <- old rdi
new rdx (a2 = arg1) <- old rsi
new rcx (a3 = arg2) <- old rdx
new r8  (a4 = arg3) <- r10        ; <-- the r10 -> rcx-equivalent shift
new r9  (a5 = arg4) <- r8
stack   (a6 = arg5) <- r9
```

Doing this once in assembly keeps `syscall_dispatch` a plain `extern "C" fn` — no inline-asm shim, no per-handler trampolines. The trade is six register moves per syscall, which is noise next to the SYSCALL/SYSRET instructions themselves.

### 5. Dispatch is a fixed-size array, not a `match`

A 512-entry array of function pointers gives O(1) dispatch with a single bounds check. A `match` over hundreds of arms would compile to a jump table anyway, but with extra bookkeeping and worse cache behavior on the cold path. Linux uses the same shape (`sys_call_table[]`). Unused slots point at `enosys` so an out-of-range read still yields a valid call and a defined error.

### 6. `SyscallResult = Result<i64, Errno>` collapsed at the boundary

Handlers reason in `Result`; the dispatcher flattens to a signed `i64` exactly once before handing the value back to assembly. Negative-errno is a userspace-ABI concern, not a kernel-internal one, and confining the conversion to one place means a handler can never accidentally return `-2` as a "success." This is the single most common source of subtle ABI bugs in kernels that grew the convention organically.

### 7. Boundary validation in `handlers.rs`, full memory checks elsewhere

`handlers.rs` rejects null pointers and pointers above `USER_VA_MAX` (the canonical-address split at `0x0000_8000_0000_0000` under 4-level paging). It does **not** walk page tables, check write permissions, or fault in pages. That is the memory agent's job, and it has to handle page faults during the access itself — there is no way to make this fully race-free with a pre-check on a multi-CPU system. Pre-validation here exists only to reject pointers that are *obviously* nonsense before we pay the cost of dispatching to the subsystem.

### 8. `fd` validation is a range check, not a table lookup

The Linux `int` for `fd` means values above `i32::MAX` are immediately invalid and earn `-EBADF`. Whether a given valid-range `fd` is *actually open* is a question only the per-process file table can answer, and that table lives in the process subsystem. So we range-check here and defer.

### 9. `fork` not `clone`

Linux internally implements `fork(2)` as `clone(SIGCHLD, ...)`, and modern glibc calls `clone` directly. We expose the raw `fork` syscall number anyway because (a) the Linux ABI still defines it, (b) Tier 1 is intentionally narrow, and (c) `clone`'s flag space is large enough to deserve its own tier. When a later tier adds `clone(2)` (nr 56), it can subsume `fork` internally without breaking the number we expose here.

### 10. `exit` is thread-exit, not process-exit

Linux's `exit(2)` (nr 60) terminates only the calling thread. `exit_group(2)` (nr 231) terminates the whole process and is what userspace libc actually calls on `exit(3)`. Tier 1 implements the thread-exit semantics because that is what nr 60 means in Linux. Anyone calling raw nr 60 from a multithreaded process and expecting whole-process termination has the same bug on gkern as on Linux — which is the point of ABI compatibility.

### 11. `extern "Rust"` declarations under cfg flags

Subsystems owned by other agents (VFS, process manager) are referenced via `extern "Rust" { ... }` declarations gated on cargo features. Until those features are enabled, handlers return `ENOSYS`. This lets `src/syscall/` build and be tested in isolation without forcing me to invent (and then unwind) stub implementations of code I do not own. When the sibling agents land their work, they enable the feature flag and the existing handler routes through to the real subsystem with no edits here.

### 12. `SYSCALL_TABLE_SIZE = 512`

Linux's x86_64 table currently uses ~450 entries with room reserved up through the 500s. 512 is the next power of two above that, which makes the bounds check a single `cmp` against an immediate and lets future tiers grow without resizing the table. Larger sizes (1024, 4096) cost rodata but buy nothing.

### 13. Single `init()` entry point in `mod.rs`

Bring-up has a strict order: install handlers, *then* arm the MSRs. If the order flipped, the very first SYSCALL after `wrmsr LSTAR` could land in an empty dispatch table. One public function enforces the order so bring-up code (owned by another agent) cannot get it wrong. `init()` is `unsafe` and documents the CPL 0 + long-mode contract — see [Phase 2 integration changes](#phase-2-integration-changes).

---

## Phase 2 integration changes

Phase 2's job for this subsystem was "compile under `no_std`, wire correctly to the CPU." Findings and changes, all confined to `src/syscall/`:

### a. Entry trampoline moved from `entry.asm` (NASM) to `entry.rs` (`global_asm!`)

**Why this was mandatory, not cosmetic.** `gkern` builds with `cargo build --target x86_64-unknown-none` and has **no `build.rs` and no NASM step** (Agent 7's integration files are `Cargo.toml`, `src/main.rs`, `.cargo/config.toml`, `rust-toolchain.toml` — none assemble `.asm`). A standalone `entry.asm` would therefore never be assembled or linked: `syscall_entry` would not exist in the kernel binary, and `IA32_LSTAR` would be armed to point at nothing. The trampoline is now embedded via `global_asm!` (AT&T syntax, `options(att_syntax)`), exactly matching the established in-repo convention in `src/scheduler/context.rs`. The instruction sequence is a faithful port of the Phase 1 NASM (same stack swap, same trap-frame layout, same Linux→SysV register shuffle, same `sysretq`). `entry.asm` was removed to keep a single source of truth.

### b. MSR programming moved into Rust `init()` (`entry::init_cpu`)

The four SYSCALL MSRs (`EFER.SCE`, `STAR`, `LSTAR`, `FMASK`) are now programmed from Rust via `rdmsr`/`wrmsr` inline `asm!`, reachable through the public `syscall::init()`. The win: `IA32_LSTAR` is written with `syscall_entry as usize as u64` — the address comes **directly from the linked symbol**, so it can never drift from the code it names (the old NASM hand-computed it with `lea`/`shr`). `EFER` is read-modify-written so the boot agent's `EFER.LME/LMA/NXE` survive; we only set `SCE`.

### c. CPL 0 + long mode (privilege requirement — documented per the constraint)

`rdmsr`/`wrmsr` are **privileged instructions**: executing them at any CPL other than 0 raises `#GP(0)`. They also assume the CPU is already in 64-bit long mode. Both conditions hold where Agent 7 calls `syscall::init()` from `kernel_main` (ring 0, long mode established by the boot agent). This is why `init()` and `init_cpu()` are `unsafe` and spell the contract out in their doc comments. **MSR writes must happen at CPL 0, after long mode — never from userspace.**

### d. `no_std` audit

No `std::`, `use std`, or `extern crate std` appears anywhere in `src/syscall/` — handlers already used `core::` (`core::mem`, `core::convert::Infallible`) and the new `entry.rs` uses only `core::arch`. The module carries `#![cfg_attr(not(test), no_std)]` to document intent and allow host unit tests, but note: a crate-level attribute on a non-root *module* is inert; the **authoritative `#![no_std]` belongs to the crate root (`src/main.rs`, Agent 7)**. None of the sibling subsystems put `#![no_std]` in their `mod.rs` either, for the same reason. The real guarantee is the absence of `std` usage, which holds.

### e. Syscall numbers verified against the Linux x86_64 ABI

`read=0, write=1, open=2, close=3, fork=57, execve=59, exit=60` — all match `arch/x86/entry/syscalls/syscall_64.tbl`. No drift. Unchanged.

### Cross-agent notes

- The `extern "C"`-symbol contract with the rest of the kernel is unchanged: assembly still calls `syscall_dispatch`, and the entry symbol is still named `syscall_entry`. Only the *source form* changed (NASM → `global_asm!`).
- The GDT/`STAR_VALUE` and per-CPU `GS:[0]` assumptions in the [interfaces table](#interfaces-this-subsystem-expects-from-other-agents) are unchanged — they moved verbatim into `entry.rs`.

---

## Interfaces this subsystem expects from other agents

| Provided by                 | Symbol / fact                                                                  | Used where                  |
| --------------------------- | ------------------------------------------------------------------------------ | --------------------------- |
| Memory / CPU bring-up agent | Per-CPU area mapped, `GS_BASE` pointing at it, `GS:[0] = kernel_rsp`           | `entry.rs` trampoline prologue |
| Memory / GDT agent          | GDT layout matching the STAR value: kernel CS at 0x08, user CS64 at 0x28+RPL3  | `entry.rs` `STAR_VALUE`     |
| VFS agent (feature `vfs`)   | `vfs_read`, `vfs_write`, `vfs_open`, `vfs_close` with documented signatures    | `handlers.rs`               |
| Process agent (feature `process`) | `proc_fork`, `proc_execve`, `proc_exit`                                  | `handlers.rs`               |

Any change to the GDT layout, per-CPU offsets, or those Rust signatures is a coordination event between agents — it must be reflected here in lockstep.

---

## What is *not* in this subsystem

- Page-fault handling on user-pointer access — memory agent.
- File-descriptor tables, inode resolution, path walking — VFS agent.
- Process creation, address-space duplication, ELF loading — process agent.
- Sentinel handshake interception — Sentinel agent. (Note: when a SYSCALL is issued by a process that has completed a Sentinel handshake, Sentinel sees it through hooks the Sentinel agent installs; that interception is not visible from this directory and is intentionally so.)
- Signal delivery on syscall return — signals agent.

If a future bug looks like it lives in this directory but the root cause is in one of the items above, the fix belongs upstream of here.

---

*Part of Golem Linux. Copyright (c) 2026 TrueSystems LLC.*
