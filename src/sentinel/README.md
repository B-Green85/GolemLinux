# Sentinel — Kernel-Resident Agentic Process Oversight

**Subsystem:** `src/sentinel/`
**Kernel:** gkern (Rust, x86_64, hybrid microkernel)
**Reference data model:** `sentinel-core` (userspace counterpart — schema-compatible)

> *To the agent, this is a standard process registration handshake.*
> *To Golem, it is a handshake with a handcuff.*

This document records every design decision in the Sentinel subsystem, why it was made, and the security property it preserves. Every claim in `GOLEM_RUNTIME_ARCHITECTURE.md` about Sentinel maps to a specific module and function in this directory.

---

## 1. What Sentinel Is

Sentinel is the kernel subsystem that

1. **Demands a registration handshake from every LLM/agent process** before its first user-visible syscall returns. (`handshake.rs`)
2. **Passively monitors** registered agents for four classes of degradation. (`monitor.rs`)
3. **Records every consequential action** in an append-only, SHA-256-chained audit log. (`audit.rs`)
4. **Is invisible** to agent processes — from an agent's view of the kernel, Sentinel does not exist. (`invisible.rs`)

These four properties are implemented as four files. They are wired together into a single facade in `mod.rs`.

---

## 2. Module Map

| File             | Responsibility                                  | Public surface (selected)                                                                 |
| ---------------- | ----------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `audit.rs`       | Append-only SHA-256-chained audit log + SHA-256 | `AuditTrail::record`, `AuditTrail::verify`, `AuditTrail::snapshot`, `sha256_hex`         |
| `handshake.rs`   | Agent registry + handshake protocol             | `HandshakeRequest`, `HandshakeAck`, `Registry::register`, `PermissionTier`               |
| `monitor.rs`     | Four-signal passive monitor + tier evaluation   | `Monitor::observe`, `Monitor::record_signal`, `Thresholds`, `ResponseTier`, `SignalKind` |
| `invisible.rs`   | Caller-context classifier + authorization gate  | `CallerContext`, `Operation`, `gate`, `InvisibleError`                                    |
| `mod.rs`         | Facade, global `SENTINEL`, init, SpinLock, clock | `Sentinel`, `SENTINEL`, `init`, `SentinelError`, `AgentStatus`                            |

---

## 3. Design Decisions

### 3.1 Kernel-resident, not userspace

> *"...not as a userspace daemon, not as an optional service, but as a non-negotiable kernel-level primitive."*

A userspace Sentinel daemon — even a privileged one — can be killed (SIGKILL from a sufficiently-privileged compromise), starved (CPU/memory pressure attack), or have its IPC channel intercepted (ptrace, LD_PRELOAD on a sidecar). A kernel-resident Sentinel has none of those failure modes: there is no PID to kill, no scheduler hint to starve, no syscall path it doesn't own.

The cost is that Sentinel runs in privileged context. We pay that cost on purpose. The trade-off is recorded in two compensations:

- **Minimal dependency surface.** No external crates. The SHA-256 implementation is embedded (`audit.rs`); the synchronization primitive is a 30-line `SpinLock` (`mod.rs`). A subsystem that audits everything else cannot have a transitive dependency on something we audit *with*.
- **Safe-Mode-only configuration.** Sentinel cannot be reconfigured from the running OS. The only authorization context for configuration changes is `CallerContext::SafeMode`, which is set by the boot-time recovery environment and is unforgeable from a running session.

### 3.2 `no_std + alloc`

The kernel doesn't ship `std`. The Sentinel modules use:

- `core::*` for primitives (atomic, cell, hint).
- `alloc::*` for collections (`Vec`, `BTreeMap`, `VecDeque`, `String`).

There is no `std::` reference anywhere in `src/sentinel/` — audited in Phase 2, see § 8. The modules compile into gkern against the bare-metal target `x86_64-unknown-none`.

**The `no_std` attribute.** `mod.rs` carries `#![cfg_attr(not(test), no_std)]` at its top. The authoritative crate-level `#![no_std]` lives in the crate root (`src/main.rs`); a crate-level attribute on a *non-root module* is inert (the compiler emits a benign "the `#![no_std]` attribute can only be used at the crate root" note and ignores it). We restate it anyway to declare the contract where the subsystem begins, matching the sibling subsystems (memory, scheduler, syscall, fs all carry the same line). The `cfg_attr(not(test), …)` form — rather than a bare `#![no_std]` — is deliberate: it leaves the `#[cfg(test)]` units free to build against a host `std` test harness should a lib target ever be introduced (see § 6).

### 3.3 SpinLock, not async, not std::sync::Mutex

Sentinel runs on the syscall hot path. Async would require a runtime (we don't have one in the kernel). `std::sync::Mutex` would require `std` (we don't have that either, and it would also call into the OS scheduler — circular). A spinlock is the right primitive: critical sections are bounded, preemption around them is controlled by the kernel, and the implementation is small enough to read in a sitting.

The implementation is in `mod.rs::SpinLock`. It uses `AtomicBool` with `Acquire`/`Release` ordering, and `core::hint::spin_loop()` while contended. RAII guard.

### 3.4 SHA-256 embedded, not a crate

The audit log's tamper-evidence property depends on the hash function. If the hash came from a crate, then a compromise of that crate (or its transitive deps, or the build of those deps) would undermine the audit trail — and the audit trail is what we'd use to *detect* a compromise. That circularity is unacceptable. We embed FIPS 180-4 directly in `audit.rs::sha256_hex`. The implementation is byte-identical to `sentinel-core`'s and is verified against the standard test vectors (`""` and `"abc"`) in `audit.rs::tests::sha256_known_vectors`.

### 3.5 Audit chain semantics

Every entry has:

```text
sequence | timestamp | actor | action | target | prev_hash
```

The entry's `hash` is the SHA-256 of those six fields concatenated with `|`. `prev_hash` is the previous entry's `hash`, or 64 zero-bytes-as-hex for the genesis row.

Properties:

- **Append-only.** There is no public `remove`, `truncate`, or `update`. The internal `Vec` is private behind a `SpinLock`. The only mutation is `push`, called from `record`.
- **Forward-chained.** Any modification to an earlier entry breaks the `prev_hash` of every subsequent entry. `AuditTrail::verify` walks the chain and finds the first broken link.
- **Deterministic.** Pass the same inputs in the same order and you get the same chain. We feed `timestamp` as a parameter (not read inside `record`) so the function is a pure function of its inputs — testable and reproducible.

Tested in `audit.rs::tests::chain_detects_tampering`.

### 3.6 The Invisibility Boundary

This is the architectural commitment that makes Sentinel different from "an auditing kernel module". Sentinel is not just *protected* from agent processes; it is *not present* in their world model.

**Implementation:** every public method on `Sentinel` (`mod.rs`) takes a `CallerContext` and routes through `invisible::gate`. `gate` consults a small authorization matrix (`invisible::is_authorized`):

| Operation     | Agent | Operator | SafeMode | Kernel |
| ------------- | :---: | :------: | :------: | :----: |
| Handshake     |   ✔   |    ✔     |    —     |   ✔    |
| Monitor       |   —   |    —     |    —     |   ✔    |
| ReadStatus    |   —   |    ✔     |    ✔     |   ✔    |
| ReadAudit     |   —   |    —     |    ✔     |   —    |
| Configure     |   —   |    —     |    ✔     |   —    |
| VerifyAudit   |   —   |    —     |    ✔     |   ✔    |

When an Agent attempts a denied operation:

1. The gate **records an `invisible_probe` entry in the audit log**, silently. The agent cannot observe this happened.
2. The gate **returns `InvisibleError::NotPresent`**, which the syscall boundary translates to `ENOSYS` (errno 38). This is the same errno an unimplemented syscall slot returns — see `syscall::dispatch::enosys`. The agent sees a vanilla kernel.

The collision between "Sentinel says no" and "syscall not implemented" is **deliberate and load-bearing**. There is a test (`invisible.rs::errno_invisibility_collision_is_intentional`) that pins the constant to ENOSYS so a future refactor can't split them apart.

**What invisibility is NOT:**

- It is not symbol-table obfuscation.
- It is not filtering Sentinel out of `/proc`.
- It is not a stealthy timing pattern.

Those are cosmetic. Real invisibility means: there is no observable difference, from an Agent's syscall surface, between "Sentinel exists and is refusing me" and "Sentinel does not exist". The ENOSYS unification *is* that property.

**What invisibility IS at the cost of one bit:** agents can attempt the Handshake syscall (`Handshake` is authorized for Agents in the matrix). This is necessary — the whole protocol depends on it — and it is the only path through the gate that succeeds for an Agent. From the agent's perspective, this looks like a generic "register-this-process" call. The agent learns nothing about Sentinel that wasn't already implied by the existence of a generic registration call.

### 3.7 The Handshake — "with a handcuff"

A handshake takes:

```rust
HandshakeRequest {
    agent_id: String,
    tier: PermissionTier,
    heartbeat_interval_secs: u64,
}
```

and returns:

```rust
HandshakeAck {
    agent_id: String,
    token: String,      // first 32 hex chars of the audit hash
    registered_at: u64,
}
```

The token is **cosmetic from the agent's perspective**. It grants the agent nothing it didn't already have; it does not unlock any privileged operation; it is not a session key. It exists only so the protocol shape resembles other registration protocols the agent's authors may be familiar with. Internally, the token is the truncated audit hash of the handshake entry — useful for the operator to correlate the agent's reported token to a line in the audit log.

The interesting work happens **after** `register` returns:

- An `AgentRecord` is allocated in kernel memory (`handshake::Registry`). Userspace cannot enumerate it (no `/proc/sentinel/agents` exists — that would violate invisibility).
- The task's `CallerContext` flips from `Operator` to `Agent`. This is a sticky property: a registered agent remains an Agent for the lifetime of the task, across `fork`, `execve`, and `setuid`. The classification is task-bound, not credential-bound.
- An audit entry is written.

After registration, every Sentinel API except `Handshake` returns ENOSYS to that task. The handcuff is closed.

**Re-registration is rejected.** Returning `HandshakeError::AlreadyRegistered` to a duplicate `agent_id`. We do not let an agent "reset" its record — that would defeat the rolling monitor window.

**Input validation.** Agent IDs are capped at 128 bytes; heartbeats clamp to [1, 3600] seconds. The kernel is the wrong place to deal with hostile-sized inputs.

### 3.8 Passive Monitoring

`monitor.rs` is *passive*: it observes, scores, and reports. It does not block syscalls, rewrite output, or talk to the agent. Response actions (pause, downgrade, lock) are decisions the parent `Sentinel` facade applies on top of the monitor's verdict — see `Sentinel::apply_tier` (`mod.rs`).

**The four signals**, with the per-signal default thresholds:

| Signal              | Detects                                              | Default |
| ------------------- | ---------------------------------------------------- | :-----: |
| RepetitionScore     | Semantically identical outputs across the window     |   0.6   |
| SelfReferentialLoop | Agent announces actions it never executes            |   0.5   |
| TokenVelocityStall  | High output volume with zero task-state progression  |   0.5   |
| ToolRetryAnomaly    | Identical tool calls with identical arguments repeat |   0.4   |

Each signal is a cheap O(window) computation over a `VecDeque<Observation>`. "Semantically identical" is approximated as "same SHA-256 of normalized output text" (see `Monitor::normalize_output_hash`). This is intentionally simple — running a semantic comparator in the kernel is the wrong design. A privileged Operator-context userspace daemon can run richer detectors and feed verdicts in through `Monitor::record_signal`.

**Cumulative scoring.** Each signal's contribution to the cumulative score is zero unless it crosses its individual gate. Above gate, its contribution is `(score - gate) / (1 - gate)`. The four contributions are summed and capped at 1.0. This shape — gate first, then rescale — prevents four mildly-elevated signals from tripping Hard tier when none of them individually warrants concern. Tested in `monitor.rs::tests::combine_zeros_signals_below_gate`.

**Tier mapping.** The cumulative score → tier mapping is:

| Tier    | Cumulative | Action                                            |
| ------- | :--------: | ------------------------------------------------- |
| None    |   < 0.4    | Continue.                                         |
| Soft    |    ≥ 0.4   | Pause. Retain permissions. Audit entry.           |
| Medium  |    ≥ 0.7   | Downgrade to `READ_ONLY`. Webhook fires.          |
| Hard    |    ≥ 0.9   | Revoke permissions. Lock agent. Audit entry.      |

Lock is irreversible from a running OS — only Safe Mode can clear the `locked` flag. The `original_tier` field is preserved on the agent record so Safe Mode can see what was revoked.

### 3.9 Safe Mode is the only configuration context

`Thresholds` is mutable only via `Sentinel::configure`, which the gate authorizes only for `CallerContext::SafeMode`. The `Configure` row in the authorization matrix has only one ✔. This is enforced in code, not policy: an Operator process cannot call `Sentinel::configure` successfully. The result is `EPERM` for Operators, `ENOSYS` for Agents.

Configuration changes are themselves audit events. They appear in the chain with `actor="safe-mode"` and `action="configure"`.

### 3.10 The Singleton + `init`

`SENTINEL` is the single instance of `Sentinel` per kernel. It is `const`-constructible (no allocations at construction time) so it can live in a `static`. `init()` is idempotent and writes a `boot` entry to the audit chain so every chain has a stable, identifiable genesis. The boot entry also lets a Safe-Mode chain-export tool detect "this is the same kernel session" vs "the kernel rebooted".

The `INITIALIZED` flag prevents double-init from accidentally writing a second genesis row.

### 3.11 Time

Kernel-side, we do not have `SystemTime` for free. The implementation uses a monotonic `AtomicU64` tick counter (`mod.rs::TICKS`, `mod.rs::now`). At bring-up the kernel calls `install_clock(unix_seconds_now)` after the RTC driver comes online. Until then, the counter increments by one per call so the audit chain still gets monotonic timestamps and `verify` can do its job.

The audit module *takes timestamp as a parameter* — it does not read a clock internally. This makes `AuditTrail::record` a pure function of its inputs and trivially unit-testable.

---

## 4. Security Model

### 4.1 Threat model

**Primary adversary:** a compromised or degraded LLM process that has completed the Sentinel handshake. Has `Agent` `CallerContext`. May have any tier of permission up to `EXECUTE`. May attempt to:

- Disable Sentinel.
- Tamper with the audit log.
- Reconfigure thresholds to avoid detection.
- Discover whether it is being monitored.
- Re-handshake under a fresh agent_id to wipe its degradation window.

**Secondary adversary:** an Operator-context process that has escalated against another Operator-context process. May try to read or mutate Sentinel state without going through Safe Mode.

**Not in scope:** physical attacks on the running machine (Hardened deployment addresses these with USB/RJ45-only configuration transfer). Kernel exploits that subvert privilege level entirely (gkern itself is the trust boundary at that point).

### 4.2 Properties Sentinel preserves

1. **Invisibility to Agent context.** Any Sentinel API except `Handshake` returns `ENOSYS` to an Agent. The error is indistinguishable from an unimplemented syscall slot. Tested in `mod.rs::tests::agent_cannot_see_status`.

2. **Invisibility under probing.** Asking about a registered agent vs a non-existent agent returns the same error code to Agent callers (`ENOSYS`, not `ENOENT`). An Agent cannot enumerate other agents by guessing IDs and watching errno. Tested in `mod.rs::tests::unknown_agent_still_invisible_to_agent`.

3. **Audit immutability.** No public mutator exists for the audit chain other than `record`. The internal `Vec` is private. `verify` detects any tampering. Tested in `audit.rs::tests::chain_detects_tampering`.

4. **Configuration restricted to Safe Mode.** `Configure` is authorized only for `CallerContext::SafeMode`. Tested in `mod.rs::tests::safe_mode_can_configure_thresholds`.

5. **No re-registration escape.** An agent cannot wipe its degradation window by re-registering under the same ID. Tested in `handshake.rs::tests::re_registration_is_rejected`.

6. **Lock irreversibility from running OS.** Once an agent crosses Hard tier and is locked, only Safe Mode can clear the flag. `Registry::lock` sets both `locked = true` and `tier = ReadOnly`; there is no `unlock` method exposed outside Safe Mode.

7. **Silent audit of probes.** Every Agent attempt at a denied operation generates an `invisible_probe` audit entry, recording the operation that was attempted. The Agent cannot observe that this happened. Tested in `mod.rs::tests::agent_query_attempts_are_audited_silently`.

### 4.3 Properties Sentinel does NOT preserve (and why)

- **It does not encrypt the audit log.** Encryption protects confidentiality; the audit log's job is *tamper-evidence*, not secrecy. SHA-256 chaining is the right primitive. (If a deployment needs the chain stored confidentially after the fact, that is a Safe-Mode export concern, not a Sentinel-runtime concern.)

- **It does not rate-limit agent syscalls.** Rate limiting would conflict with the "Golem does not restrict what you run" principle in the architecture doc. The monitor *records* abnormal patterns; the response *downgrades or locks*; neither imposes pre-emptive limits on healthy agents.

- **It does not vet the contents of agent prompts or outputs at the kernel level.** Content vetting would require a semantic model in the kernel — the wrong place. The four signals are structural, not semantic. Userspace detector daemons can do richer analysis and push verdicts in via `record_signal`.

---

## 5. Interactions With the Rest of gkern

### 5.1 Syscall layer

`syscall::dispatch` provides the entry point. Three Sentinel-related syscalls are reserved (numbers TBD by the kernel team):

- `sys_sentinel_handshake` → `Sentinel::handshake`
- `sys_sentinel_status` → `Sentinel::status` *(operator-only; ENOSYS for agents)*
- `sys_sentinel_heartbeat` → `Sentinel::heartbeat` *(kernel-internal)*

All three are routed through the syscall dispatch table. The dispatcher's existing `ENOSYS` fallback (`syscall::dispatch::enosys`) is **the same return path** an Agent gets when it tries any denied Sentinel operation. This is the invisibility property in action — the agent cannot tell from errno alone whether a slot is unimplemented or refused.

`CallerContext` classification happens in the syscall entry: when a task's `task_struct` carries a `sentinel_registered = true` flag, the dispatcher sets `CallerContext::Agent` before calling any Sentinel API. Otherwise it sets `Operator`. `SafeMode` is set by the boot environment. `Kernel` is the default for internal call sites.

### 5.2 Scheduler

The scheduler is the heartbeat driver. On each scheduler tick, registered agents whose `last_heartbeat + heartbeat_interval_secs < now` get a heartbeat recorded (or a missed-heartbeat audit entry — to be implemented when the scheduler hook lands).

### 5.3 Output hook

A small kernel-side hook on agent stdout/stderr/IPC computes:

- `Monitor::normalize_output_hash(chunk)` for repetition scoring.
- Token count (cheap: byte length / average-token-bytes).
- Tool-call hash if the chunk is a structured tool call.

These are fed to `Sentinel::observe`. The hook **does not block** — it computes and forwards.

### 5.4 Safe Mode boot

Safe Mode's only job, with respect to Sentinel, is to allow `read_audit` and `configure`. It does so by setting `CallerContext::SafeMode` for the Safe-Mode shell process. From that shell, the operator can:

- Walk the audit chain (`read_audit`).
- Verify chain integrity (`verify_audit`).
- Adjust thresholds (`configure`).
- Clear an agent's `locked` flag (this is the only legitimate path to unlock; implementation lives outside this module — Safe Mode is its own subsystem).

---

## 6. Testing

Every module ships with `#[cfg(test)]` units. Coverage summary:

- **`audit.rs`**: SHA-256 standard test vectors; chain record + verify; chain tamper detection; hash uniqueness across identical inputs (proves the chaining works).
- **`invisible.rs`**: Agent ENOSYS; Agent handshake permitted; Operator config denied outside Safe Mode; Safe Mode config permitted; ENOSYS errno locked at 38.
- **`handshake.rs`**: First-registration success; re-registration rejected; empty ID rejected; oversized ID rejected; heartbeat bounds; heartbeat updates record; lock semantics; token truncation.
- **`monitor.rs`**: Fresh agent → None; repetition crosses Soft; forget clears state; tool retry detection; normalize collapses whitespace/case; tier threshold edges; combine gates correctly; injected signal drives tier.
- **`mod.rs`** (facade integration): Agent handshakes and gets token; Agent denied status; Operator gets status; probes audited silently; Hard tier locks agent; Safe Mode configures; chain verifies after traffic; deregister clears monitor state; unknown agent ENOENT to Operator but ENOSYS to Agent; injected signal drives facade tier.

**How the units are verified.** The integration crate root (`src/main.rs`) is a `#![no_std] #![no_main]` *binary* with no lib target, so `cargo test -p gkern --lib sentinel` does not apply to the assembled kernel (it reports "no library targets found"). The `#[cfg(test)]` units above are retained as the authoritative behavioral spec and are written to run unmodified under a host `std` harness if the subsystem is ever split into a testable lib (this is what the `#![cfg_attr(not(test), no_std)]` gate in § 3.2 enables). In Phase 2, correctness is established by:

1. **The `no_std` kernel build** — `cargo build` against `x86_64-unknown-none` compiles the whole Sentinel subsystem with no `std`, which is the operative compliance check.
2. **SHA-256 + chain re-verification** — the embedded hash was re-validated against the FIPS 180-4 vectors (`""`, `"abc"`, and the standard "quick brown fox" vector) and the chain's forward-tamper detection was re-checked out-of-tree during the port. The in-tree `audit.rs::tests::sha256_known_vectors` and `chain_detects_tampering` pin the same properties for the future host harness.

---

## 7. Out of Scope (Future Work)

Documented here so future contributors don't think these are missing-but-overlooked:

- **Persistence.** The audit chain currently lives in kernel memory. An `audit-flush` worker that streams entries to a Safe-Mode-readable disk region is a separate subsystem. The chain's append-only + hash-chained shape was chosen specifically to make that worker's job simple — it can flush in order, and Safe Mode can verify each batch.
- **Webhook fires.** The Medium tier action specifies a webhook. The webhook delivery mechanism is a Safe-Mode-configured Operator-context daemon that subscribes to tier-change audit entries. Out of scope for the kernel module.
- **Richer detectors.** The four signals are intentionally simple. Userspace detector daemons running in Operator context can feed verdicts in via `record_signal`. Building those daemons is out of scope here.
- **Sentinel state migration.** "Did anything change at the kernel level or with Sentinel since last session?" is implemented by Safe Mode comparing the on-disk Sentinel config against the running config. The Safe Mode subsystem owns that comparison; this module just makes the running config readable from `SafeMode` context.

---

## 8. Phase 2 Integration Notes

Phase 2's job for this subsystem was: *compile under `no_std`, and guarantee Sentinel initializes before any other subsystem.* All changes were confined to `src/sentinel/`. Findings and changes:

### 8.1 `no_std` audit — clean

Every file in `src/sentinel/` was audited for `std::` usage. **There is none** outside doc-comments. Phase 1 was already written against `core` + `alloc`:

- `core::{cell::UnsafeCell, sync::atomic, hint::spin_loop, ops}` for the `SpinLock`, the clock, and the guards.
- `alloc::{string, vec, collections::{BTreeMap, VecDeque}, format}` for the registry, the audit chain, and the monitor buffers.

No replacements were required. The subsystem compiles into `gkern` against `x86_64-unknown-none` with `cargo build`.

### 8.2 `#![no_std]` declaration

`mod.rs` now carries `#![cfg_attr(not(test), no_std)]` at its top (§ 3.2), matching the memory / scheduler / syscall / fs subsystems. The authoritative `#![no_std]` remains in the crate root (`src/main.rs`); the module-level restatement is the documented subsystem convention and emits only the expected benign "can only be used at the crate root" note.

### 8.3 Initialization order — Sentinel is FIRST (hard requirement)

**The invisibility gate must be live before the scheduler, the syscall interface, or the filesystem initialize.** If a syscall path went live before the gate, there would be a window in which a Sentinel operation could dispatch without `CallerContext` classification — the load-bearing security property of the whole subsystem (§ 4.2.1). If the scheduler ran tasks before the gate, an Agent task could issue its first syscall outside Sentinel's authority. Initializing the gate first closes that window entirely.

`sentinel::init()` is designed to make "first" trivially safe: it takes no arguments, cannot fail, and depends on **no other subsystem** — not the heap (the singleton is `const`-constructed into a `static`; the audit chain's first `Vec`/`String` allocations are the only allocator use, and the genesis `boot` entry is small), not the clock (it uses the monotonic tick fallback until `install_clock` runs), not the scheduler. That is precisely why it can run before everything else. The integration crate root calls it as **step 1** of `kernel_main`, ahead of `memory::init`, `fs::init`, `scheduler::init`, and the syscall wiring.

> Note for the integration agent: `sentinel::init()` must remain the first subsystem call in `kernel_main`. Reordering it after memory/scheduler/syscall reintroduces the unclassified-dispatch window and breaks the invisibility guarantee. This ordering is a security property, not a performance preference.

### 8.4 `init()` is the public entry point

`pub fn init()` in `mod.rs` is the single, clean bring-up call. It is idempotent (a second call is a no-op, so a double-init cannot write a second audit genesis row), writes the `boot` genesis entry, and verifies the empty chain to fail fast if the audit module is broken before any traffic is accepted.

### 8.5 SHA-256 audit chain under `no_std` — verified, no crypto crate

The audit chain uses the **embedded FIPS 180-4 SHA-256** in `audit.rs::sha256_hex` — no `sha2`, no `ring`, no external crypto crate, and therefore no transitive dependency on the very thing the chain exists to protect (§ 3.4). No std-dependent crate was used in Phase 1, so none had to be replaced. During the port the implementation was re-validated against the standard test vectors and the forward-chain tamper-detection was re-confirmed (§ 6). The hash is a pure function of a `&[u8]`, allocating only its returned `String`, so it is fully `no_std`-correct.

### 8.6 Invisibility preserved

No change in this phase weakened the invisibility boundary. The gate, the authorization matrix, the silent `invisible_probe` auditing, and the `NotPresent → ENOSYS (38)` collision are untouched. The only code edits were the `no_std`/`dead_code` attributes, documentation, and a test-only import gate.

---

*Copyright (c) 2026 TrueSystems LLC. All rights reserved.*
*Golem Linux is developed under the CDMAE methodology.*
