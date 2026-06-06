# Golem Linux — Safe Mode

**Audience:** security-conscious operators of Golem Linux.
**Scope:** the boot-time recovery environment that configures the Sentinel **kernel subsystem** (`src/sentinel/`).

> Read § 1 and § 2 before touching anything. Safe Mode is the *only* place Sentinel
> can be reconfigured, and the security guarantees of the whole system depend on
> understanding why that is, and what Safe Mode deliberately refuses to do.

---

## 0. A Note Before You Begin — Two Different "Sentinels"

There are **two** distinct things named "Sentinel" in this ecosystem. This document is
about exactly one of them. Conflating them will lead you to wrong operational decisions,
so the distinction is stated up front and repeated wherever it matters.

| | **Standalone Sentinel daemon** | **GolemLinux Sentinel kernel subsystem** |
| --- | --- | --- |
| **Where it lives** | The Rust workspace at `~/Projects/sentinel` | `src/sentinel/` inside the gkern kernel |
| **What it is** | A userspace **process** | Part of the kernel itself — there is no process |
| **Runs on** | Linux, macOS, Windows | Golem Linux only (it *is* the OS) |
| **Kernel visibility** | eBPF hooks | Native syscall-level interception — no eBPF needed |
| **Observer vs enforcer** | `--oo` runtime flag | **Compile-time** feature flag |
| **Transport** | Unix socket (Linux/macOS) or named pipe (Windows) | `KernelTransport` — direct kernel IPC, **no socket path** |
| **Key / config setup** | `sentinel-keygen`, `sentinel.toml` | **Safe Mode** (this document) |

They share two crates — `sentinel-types` and `sentinel-signals` — and they share a data
model, which is why the names collide. **Beyond those shared types, they diverge
completely.** They are not the same program in two deployments; they are two programs.

**Safe Mode exists for the kernel subsystem only.** The standalone daemon has its own
configuration mechanisms (`sentinel.toml` for settings, `sentinel-keygen` for secrets at
install time) and never enters Safe Mode — it cannot, because it is a process on a host
OS, not a recovery environment. If you came here looking to configure the standalone
daemon, **this is the wrong document**: edit `sentinel.toml` or run `sentinel-keygen` in
that workspace instead.

Everything below concerns the kernel subsystem.

---

## 1. What Safe Mode Is, and Why It Exists

Safe Mode is a **separate, minimal, agent-free recovery environment** built into Golem
Linux. It boots its own minimal userland with one job: to let an operator read and change
the configuration of the Sentinel kernel subsystem.

### Why it has to exist

The Sentinel kernel subsystem is, by design, **invisible and immutable from the running
main OS**. From inside a normal Golem session:

- Sentinel does not answer configuration queries — every config call returns `ENOSYS`
  (errno 38), the exact error an unimplemented syscall returns. To a process in the main
  OS, Sentinel is indistinguishable from a kernel that has no Sentinel at all.
- Thresholds cannot be edited. Locked agents cannot be unlocked. The audit chain cannot
  be walked.

That immutability is a feature: it means a compromised or degraded agent — even one
running with the highest permission tier — has **no API surface** through which to disable
its own oversight, rewrite the detection thresholds, or clear its own lock. (See
`src/sentinel/README.md` § 4 for the threat model.)

But configuration has to live *somewhere*. If Sentinel could never be reconfigured, it
would be unusable; if it could be reconfigured from the running OS, the immutability
guarantee would be a lie. Safe Mode resolves that tension by moving the *only*
configuration authority out of the running OS entirely and into a boot-time environment
that **cannot contain an agent to abuse it**.

In short: **Safe Mode is where Sentinel becomes mutable, precisely because it is the one
place an agent can never be.**

---

## 2. The Security Model — No Agents, No LLM Registration, Ever

This is the load-bearing property. Read it carefully.

### 2.1 The invariant

> **Safe Mode contains no LLM processes, no agents, and no path to create one.
> Not "agents are disabled." Not "agents are sandboxed." There is no agent, and no way
> to make one.**

Concretely, in Safe Mode:

- **No LLM processes run.** None are started at boot; none can be started by the operator.
- **No registration handshake is reachable.** In the Sentinel authorization matrix, the
  `Handshake` operation is the *one* operation an Agent context may invoke in the main OS.
  In Safe Mode context it is explicitly **denied**:

  ```
  (SafeMode, Handshake) => false   // Safe Mode is agent-free, by definition.
  ```

  (`src/sentinel/invisible.rs`.) Because registration is the only act that flips a task's
  classification to `Agent`, and registration is refused in Safe Mode, **no task in Safe
  Mode can ever become an Agent.** The classification simply has no way to be assigned.

- **No general OS services run.** No network services beyond the minimum the configuration
  task itself requires. No agentic tooling. No application stack. The environment is
  deliberately barren.

### 2.2 Why "ever" is literal

The word *ever* is not rhetorical. Three independent mechanisms enforce it:

1. **The authorization matrix denies `(SafeMode, Handshake)`.** Registration — the sole
   gateway to `Agent` classification — cannot succeed in Safe Mode. This is enforced in
   code, with a test (`safe_mode_can_configure` and the matrix in `invisible.rs`), not by
   convention.

2. **`SafeMode` context is set by the boot environment and is unforgeable from a running
   session.** A process in the main OS cannot assign itself `CallerContext::SafeMode`; the
   classification is established by the boot-time recovery environment before any userland
   the operator can reach comes up. Conversely, the configuration capability that
   `SafeMode` grants is **unavailable** to the running OS — there is no syscall, flag, or
   escalation that produces `SafeMode` context outside of an actual Safe Mode boot.

3. **The environment ships nothing to run an agent with.** Even setting the gate aside,
   Safe Mode's userland does not include the runtime, models, or services an agent would
   need. It is a configuration shell, not an OS.

The result is a clean separation of powers:

| Capability | Main OS (Agent) | Main OS (Operator) | **Safe Mode** |
| --- | :---: | :---: | :---: |
| Register an agent (Handshake) | ✔ | ✔ | **✗ never** |
| Read Sentinel config / status | ✗ (ENOSYS) | partial (status only) | **✔** |
| Read / verify the audit chain | ✗ (ENOSYS) | ✗ | **✔** |
| Change thresholds (Configure) | ✗ (ENOSYS) | ✗ (EPERM) | **✔** |
| Clear an agent's `locked` flag | ✗ | ✗ | **✔** |

The exact same context that gains *full* configuration authority is the exact context that
can *never host an agent*. That is the entire point.

### 2.3 What this buys you

Because the only mutable-configuration context is also the only guaranteed-agent-free
context, the following attacks are structurally impossible — not mitigated, impossible:

- An agent cannot disable Sentinel: the config API is `ENOSYS` to it.
- An agent cannot loosen its own detection thresholds: same.
- An agent cannot unlock itself after being locked at Hard tier: unlock exists only in
  Safe Mode, where the agent cannot exist.
- An agent cannot reconfigure Sentinel by spawning a Safe-Mode-context helper: `SafeMode`
  context is unforgeable from a running session.

---

## 3. How to Boot into Safe Mode

Safe Mode is selected at boot, before the main OS userland initializes. It is not a
runlevel you switch to from a running session — by design, you cannot reach it from inside
Golem, because reaching it from inside Golem would mean a running session could assume
configuration authority.

**To enter Safe Mode:**

1. **Reboot the machine** (or power on from cold).
2. **At the boot menu, select the Safe Mode / recovery entry.** This boots the minimal,
   agent-free recovery environment instead of the main OS.
3. The recovery environment establishes `CallerContext::SafeMode` for its configuration
   shell and brings up **only** the services the configuration task needs. No agents, no
   application services, no general networking.
4. You are dropped into the Sentinel configuration interface (§ 4).

**Golem Hardened note.** In a Hardened deployment the configuration you apply in Safe Mode
is **authored on a physically separate machine** and transferred to the target via USB
data cable or wired RJ45 ethernet only — never wirelessly. The target machine *receives*
configuration; it does not generate it. Safe Mode on the target is where that received
configuration is read in and applied. (See `GOLEM_RUNTIME_ARCHITECTURE.md` → Deployment
Tiers.)

---

## 4. The Configuration Interface

Inside Safe Mode you interact with Sentinel through a small, deliberate command surface.
Every command runs in `SafeMode` context, so every one of them is authorized to touch
state that is sealed off from the main OS.

The four core commands:

| Command | What it does |
| --- | --- |
| `show` | Display the current Sentinel configuration (thresholds, per-signal gates, locked agents, and the running config the main OS will be reconciled against). |
| `set`  | Stage a change to a configuration value — e.g. a detection threshold or a per-signal gate, or clearing an agent's `locked` flag. |
| `save` | Commit the staged changes to the on-disk Sentinel configuration region and write a `configure` entry into the tamper-evident audit chain. |
| `exit` | Leave the configuration interface and continue the Safe Mode boot path (typically a reboot back into the main OS, which triggers migration — see § 5). |

### 4.1 `show`

Reads the current configuration. In `SafeMode` context the gate authorizes `ReadStatus`,
`ReadAudit`, and `VerifyAudit`, so `show` can surface:

- the active thresholds and the four per-signal gates
  (RepetitionScore, SelfReferentialLoop, TokenVelocityStall, ToolRetryAnomaly);
- which agents are currently `locked`, and the `original_tier` each held before it was
  revoked (so you can see what a lock took away);
- the integrity status of the audit chain (a `show`-time `verify` walk confirms no link
  in the chain has been tampered with).

`show` is read-only. It stages nothing and writes nothing.

### 4.2 `set`

Stages a single configuration change. Examples of what `set` can change:

- a cumulative-score tier boundary or an individual signal gate;
- the heartbeat expectations applied to agents;
- the `locked` flag on a specific agent — **clearing a lock is a Safe-Mode-only operation,
  and this is the only legitimate path to it.** Once an agent crosses Hard tier and is
  locked in the running OS, nothing in the main OS can unlock it; `set` here is where that
  decision is made by a human operator.

`set` only **stages** the change. Nothing is persisted and nothing reaches the audit chain
until you `save`. This lets you compose several `set` calls, review them with `show`, and
commit them as one reconciled change set.

### 4.3 `save`

Commits the staged changes. Two things happen, in order:

1. The new configuration is written to the **on-disk Sentinel configuration region** — the
   durable state that the main OS will reconcile against at its next boot (§ 5).
2. A configuration change is itself an **audit event.** It is appended to the SHA-256
   forward-chained audit log with `actor = "safe-mode"` and `action = "configure"`. There
   is no way to change Sentinel's configuration without leaving a permanent, tamper-evident
   record of *that you did so* in the chain. The audit log is append-only; a `configure`
   entry can never be removed or rewritten without breaking every subsequent link, which
   `verify` will detect.

If you `exit` without `save`, staged changes are discarded and the on-disk configuration is
left exactly as it was.

### 4.4 `exit`

Leaves the configuration interface. After `exit` the operator typically reboots into the
main OS. That reboot is what triggers **migration** — the reconciliation described next.

---

## 5. How Migration Works — What Happens at the Next Main OS Boot

Safe Mode does not push configuration into a running system (it can't — the running system
is, by definition, not running while you are in Safe Mode). Instead, `save` updates the
**on-disk** Sentinel configuration, and the *main OS* reconciles itself against that
on-disk state the next time it boots and a session logs in.

### 5.1 The login-time state query

Golem runs an internal state query at **every** login:

```
Did anything change at the kernel level or with Sentinel since last session?
```

This compares the on-disk Sentinel configuration (what Safe Mode last wrote) against the
running Sentinel configuration. Two outcomes:

- **No changes detected → `null`.** Nothing was changed in Safe Mode since the last
  session. Login proceeds normally. No migration runs.

- **Changes detected → migration runs *before login completes*.** If the on-disk config
  differs from the running config, the new Sentinel state is reconciled and applied
  **before** the main OS environment becomes available. The desktop/session you log into is
  not handed to you until reconciliation has fully succeeded.

> **You do not get into Golem with an unresolved Sentinel state.** Login is gated on a
> clean reconciliation. There is no "log in now, apply the security config later" path.

### 5.2 The transport — `KernelTransport`, not a socket

> **This is one of the places the two Sentinels diverge — read § 0 if you skipped it.**

The reconciliation for the **kernel subsystem** is carried over `KernelTransport`: direct
kernel IPC, with **no socket path** and no userspace daemon in the loop. This is *not* the
Unix socket / named-pipe transport the **standalone daemon** uses. If you have read older
architecture notes that describe "Unix socket migration," understand that the socket
phrasing belongs to the standalone daemon's world; the GolemLinux kernel subsystem
reconciles in-kernel over `KernelTransport`. The end-to-end guarantee is identical — the
main OS does not come up until the new Sentinel state is applied — but there is no socket
file an operator should look for, and none to secure.

### 5.3 Where the new config takes effect

Recall that **Sentinel initializes first, before every other subsystem** — this is a hard
requirement, not a performance preference (`src/sentinel/README.md` § 8.3). The
reconciled configuration is what Sentinel comes up with on that first-in-line
initialization. By the time the scheduler, syscall interface, and filesystem initialize,
Sentinel is already live with the migrated thresholds and any cleared locks. There is no
window in which the OS runs with stale Sentinel state while the new config is "still being
applied."

---

## 6. What Happens If Migration Fails

This is the fail-closed behavior, and it is intentionally severe.

> **If migration fails, init halts. There is no partial state, and there is no login.**

Sentinel is the first subsystem to initialize and the trust authority for everything above
it. If the reconciliation of the new Sentinel state cannot be completed and applied
cleanly, the only safe action is to **stop** — because every alternative leaves the machine
running under a Sentinel configuration that is neither the old one nor the new one:

- **No partial apply.** The system never adopts "some of the new thresholds." Either the
  full reconciled state is applied, or none of it is. A half-applied security
  configuration is precisely the ambiguous state an attacker would want; migration refuses
  to produce it.
- **No fallback to the old state and proceed.** Silently continuing on the previous config
  after a failed migration would mean an operator believes their Safe Mode changes are in
  effect when they are not. Migration does not do this.
- **Init halts.** Because Sentinel comes up before the scheduler and syscall layer, a
  failed migration stops the boot at the governance layer. The main OS userland never
  initializes; **you do not reach a login prompt on a machine with unresolved Sentinel
  state.**

### 6.1 What an operator does about it

A halted init is a signal, not a dead end. The recovery path is to **boot back into Safe
Mode** (§ 3) — which is always reachable from the boot menu, independent of the main OS —
and from there:

- `show` the on-disk configuration and run the audit-chain `verify` to inspect what was
  written and whether the chain is intact;
- correct the staged configuration with `set` and re-`save`, or revert to a known-good
  configuration;
- `exit` and reboot to re-attempt reconciliation.

Because Safe Mode is a separate boot-time environment that does not depend on the main OS
coming up, **a migration failure can never lock you out of the ability to fix it.** The
machine fails closed (no login, no partial security state), and the fix is always one
reboot-into-Safe-Mode away.

---

## 7. Quick Reference

- **Safe Mode is for the kernel subsystem (`src/sentinel/`) only.** The standalone Sentinel
  daemon uses `sentinel.toml` / `sentinel-keygen` and never enters Safe Mode. (§ 0)
- **No agents, ever.** `(SafeMode, Handshake)` is denied in code; `SafeMode` context is
  unforgeable from a running session; the environment ships nothing to run an agent with.
  (§ 2)
- **Enter it from the boot menu**, not from a running session. (§ 3)
- **`show` / `set` / `save` / `exit`** — read, stage, commit (+ audit entry), leave.
  Nothing persists until `save`. (§ 4)
- **Migration happens at the next main OS boot**, gated on a clean reconciliation over
  `KernelTransport` (not a Unix socket). No clean state → no login. (§ 5)
- **Failure is fail-closed:** init halts, no partial state. Recover by rebooting back into
  Safe Mode. (§ 6)

---

*See also: `src/sentinel/README.md` (kernel subsystem design + threat model),
`GOLEM_RUNTIME_ARCHITECTURE.md` (Safe Mode, migration, deployment tiers).*

*Copyright © 2026 Brandon Green. Licensed under the Apache 2.0 License.*
*Golem Linux is developed under the CDMAE methodology.*
