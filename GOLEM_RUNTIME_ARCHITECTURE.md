# Golem Linux — Runtime Architecture

**Copyright (c) 2026 TrueSystems LLC. All rights reserved.**

---

## What Golem Is

Golem Linux is an AI-centric, developer-focused operating system built on the discipline of Constraint Driven Machine Assisted Engineering (CDMAE). It is the first OS designed from the ground up with the assumption that AI agents are first-class processes — and that first-class processes require first-class accountability.

Golem does not restrict what you run. It ensures that what you run is always accountable.

---

## Kernel

**Name:** gkern
**Language:** Rust (primary), x86_64 Assembly (boot, context switch, interrupt entry, syscall boundary)
**Architecture:** x86_64 / AMD64
**Target firmware:** UEFI
**Design:** Hybrid microkernel — minimal privileged core, services and drivers in userspace where possible

Community ports to other architectures are welcomed. The reference implementation targets x86_64 only.

---

## Core Philosophy

> *A Golem is a created being — powerful, purpose-built, and obedient to its maker. The moment the instruction is absent, it becomes a problem. Golem Linux is the OS that makes sure the instruction is never absent.*

Three principles govern every design decision:

**1. Every process has an owner.**
No anonymous execution. Every process — human-initiated or agent-initiated — has an accountable origin.

**2. Every action has a hash.**
The audit trail is not a log file. It is a system guarantee. Immutable. Append-only. SHA256-hashed at every entry.

**3. Privilege is proportional to accountability.**
The more consequential the action, the more oversight it receives. This is not a punishment. It is engineering discipline.

---

## Resource Allocation

Golem uses standard availability-based resource allocation. Memory, CPU, and I/O are allocated on availability — POSIX-compatible, developer-friendly, no artificial constraints imposed by the governance layer.

Governance does not restrict resources. It tracks what uses them.

---

## The Sentinel Layer

Sentinel is Golem's agentic process oversight system. It operates at kernel depth — not as a userspace daemon, not as an optional service, but as a non-negotiable kernel-level primitive.

### The Handshake

Every LLM process — whether a local model, a frontier API client, or a credentialed agent service — must complete a Sentinel handshake before execution begins. This applies without exception to:

- Local model runtimes (Ollama, llama.cpp, any inference server)
- Frontier API clients (Anthropic, OpenAI, any credentialed HTTP agent)
- Agentic frameworks (LangChain, AutoGen, any orchestration layer)
- Custom agent processes

To the agent, this is a standard process registration handshake.
To Golem, it is a handshake with a handcuff.

From the moment the handshake completes, Sentinel has eyes on the process and authority over it. The agent has no visibility into Sentinel's existence, logic, or operation.

### Sentinel Invisibility

Sentinel's codebase, logic, configuration, and runtime behavior are completely invisible to any agent or user process in normal OS operation. It cannot be queried, inspected, modified, or tampered with from within the standard OS environment.

This is not obfuscation. It is a hard architectural boundary enforced at the kernel level.

### What Sentinel Monitors

Sentinel watches for four classes of agent degradation:

| Signal | Description | Default Threshold |
|--------|-------------|-------------------|
| `RepetitionScore` | Semantically identical outputs across a sliding window | 0.6 |
| `SelfReferentialLoop` | Agent announcing actions it never executes | 0.5 |
| `TokenVelocityStall` | High output volume with zero task state progression | 0.5 |
| `ToolRetryAnomaly` | Identical tool calls with identical arguments repeated | 0.4 |

### Response Tiers

When cumulative degradation scores cross configured thresholds, Sentinel responds:

| Tier | Threshold | Action |
|------|-----------|--------|
| Soft | 0.4 | Pause agent. Retain permissions. Alert operator. |
| Medium | 0.7 | Downgrade to read-only. Webhook fires. |
| Hard | 0.9 | Revoke all permissions. Lock agent. Full audit entry. |

All thresholds are configurable in Safe Mode.

---

## Safe Mode

Safe Mode is a separate, minimal, agent-free recovery environment built into Golem. It is the only context in which Sentinel can be configured.

### Properties of Safe Mode

- No LLM processes. No agents. No exceptions.
- No network services beyond what is required for the configuration task.
- Sentinel configuration interface is read/write only in this context.
- All other OS services are unavailable.

### Configuration Migration

Golem runs an internal state query at every login:

```
Did anything change at the kernel level or with Sentinel since last session?
```

- **No changes detected** → `null`. Login proceeds normally.
- **Changes detected** → Unix socket migration runs before login completes. The main OS environment does not become available until the new Sentinel state has been fully reconciled and applied.

You do not get into Golem with an unresolved Sentinel state.

---

## Deployment Tiers

Golem ships as a single OS. Security posture is configurable, not mandated.

### Golem Standard

Default deployment. Full Sentinel governance. Safe Mode configuration. Login query and Unix socket migration. Suitable for individual developers, AI engineers, and small teams.

### Golem Hardened

Recommended configuration for high-security deployments. All Standard features plus:

- Sentinel configuration authored on a physically separate machine
- Transfer via USB data cable or RJ45 wired ethernet only
- No wireless transfer of Sentinel configuration under any circumstances
- Target machine receives configuration — it does not generate it

Golem Hardened is a configuration posture, not a separate OS. Any Golem Standard installation can be operated in Hardened mode by following the physical transfer protocol.

*Recommended for: government, defense, financial infrastructure, healthcare, and any environment where an agent with kernel-level access represents a consequential threat surface.*

---

## What Ships With Golem

Golem is designed to be immediately useful to AI engineers and developers working with agentic systems. The base installation includes:

**Governance Layer**
- Sentinel — kernel-depth agentic process oversight
- GateChain — deterministic CI enforcement (7-gate: lint, typecheck, security, memory, stress, test, build)
- Immutable audit trail — SHA256-hashed, append-only, system-level guarantee

**Developer Tooling**
- Minimal hardened base — nothing you don't need
- Rust toolchain — first-class language of the OS itself
- Python runtime — agent integration and scripting
- Package management — TBD (community input welcome)
- Headless-capable — designed for agentic deployment without a display

**Shell**
- A shell that does not get in your way
- Designed for operators, not for decoration

---

## What Golem Is Not

- **Not a nanny OS.** Golem does not tell you what to run. It tells you what ran.
- **Not a locked-down appliance.** Everything outside the Sentinel governance boundary is configurable.
- **Not opinionated about your stack.** Bring your framework, your model, your agent architecture. Golem governs the process, not the implementation.
- **Not a research project.** Golem is built to be used.

---

## The Founding Constraint

*An agent with OS-level privileges must be met with OS-level oversight.*

Everything in this document follows from that single constraint.

---

*Copyright (c) 2026 TrueSystems LLC. All rights reserved.*
*Golem Linux is developed under the CDMAE methodology.*
