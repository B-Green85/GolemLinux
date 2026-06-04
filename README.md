# Golem Linux

**An AI-centric operating system where AI agents are first-class processes — and first-class processes carry first-class accountability.**

Golem does not restrict what you run. It ensures that what you run is always accountable.

> *A Golem is a created being — powerful, purpose-built, and obedient to its maker. The moment the instruction is absent, it becomes a problem. Golem Linux is the OS that makes sure the instruction is never absent.*

---

## The kernel: `gkern`

| | |
|---|---|
| **Language** | Rust (primary) + x86_64 Assembly (boot, context switch, interrupt entry, syscall boundary) |
| **Architecture** | x86_64 / AMD64 |
| **Firmware** | UEFI |
| **Design** | Hybrid microkernel — minimal privileged core, services in userspace where possible |
| **Runtime** | `no_std`, `no_main`, `panic = "abort"` |

The reference implementation targets x86_64 only. Community ports to other architectures are welcome.

---

## Core philosophy

Three principles govern every design decision:

1. **Every process has an owner.** No anonymous execution. Every process — human- or agent-initiated — has an accountable origin.
2. **Every action has a hash.** The audit trail is not a log file; it is a system guarantee. Immutable, append-only, SHA256-hashed at every entry.
3. **Privilege is proportional to accountability.** The more consequential the action, the more oversight it receives.

The founding constraint everything follows from:

> *An agent with OS-level privileges must be met with OS-level oversight.*

---

## Sentinel

Sentinel is Golem's agentic process oversight system. It operates at **kernel depth** — not a userspace daemon, not an optional service, but a non-negotiable kernel primitive.

Every LLM process — local model runtime, frontier API client, agentic framework, or custom agent — must complete a **Sentinel handshake** before execution begins. To the agent it looks like a standard process registration. To Golem it is a handshake with a handcuff: from that moment Sentinel has eyes on the process and authority over it, and the agent has no visibility into Sentinel's existence.

Sentinel watches for four classes of agent degradation:

| Signal | Description | Default threshold |
|--------|-------------|-------------------|
| `RepetitionScore` | Semantically identical outputs across a sliding window | 0.6 |
| `SelfReferentialLoop` | Agent announcing actions it never executes | 0.5 |
| `TokenVelocityStall` | High output volume with zero task-state progression | 0.5 |
| `ToolRetryAnomaly` | Identical tool calls with identical arguments repeated | 0.4 |

When cumulative scores cross thresholds, Sentinel escalates:

| Tier | Threshold | Action |
|------|-----------|--------|
| Soft | 0.4 | Pause agent, retain permissions, alert operator |
| Medium | 0.7 | Downgrade to read-only, fire webhook |
| Hard | 0.9 | Revoke all permissions, lock agent, full audit entry |

Thresholds are configurable only in **Safe Mode** — a separate, minimal, agent-free recovery environment that is the sole context in which Sentinel can be reconfigured.

---

## Repository layout

The kernel is one Cargo crate (`gkern`). `src/main.rs` is the integration layer: it owns the crate-level attributes, the panic handler, and the kernel entry point, then wires the subsystems together in dependency order. Each subsystem lives under `src/<name>/` with its own `README.md`.

```
src/
├── main.rs       integration layer + kernel_main entry point
├── boot/         UEFI boot — boot.asm + linker.ld (no Rust module)
├── memory/       physical frame allocator, 4-level paging, heap
├── scheduler/    process model, round-robin preemptive scheduler, context switch
├── syscall/      Linux x86_64 syscall ABI (bit-for-bit compatible)
├── fs/           VFS abstraction + RamFS (mounted at /)
└── sentinel/     kernel-depth agentic process oversight
```

Initialization order is a hard requirement (see `src/main.rs`):

1. **sentinel** — invisibility gate up before anything else
2. **memory** — heap up before anything allocates
3. **fs** — needs the heap
4. **scheduler** — needs the heap
5. **syscall** — needs the scheduler

> **Status:** the integration layer is written to the agreed boot handoff contract (`kernel_main(memory_map)` via the System V AMD64 ABI). The committed boot subsystem does not yet honor that contract — aligning `src/boot/` with it is a prerequisite for the final link. See `GOLEM_PHASE1_BUILD_NOTES.md`.

---

## Building

Golem builds on the Rust nightly toolchain against a bare-metal target. Both are pinned in-repo:

- `rust-toolchain.toml` selects nightly with `rust-src` and `llvm-tools-preview`.
- `.cargo/config.toml` sets the default target to `x86_64-unknown-none` and passes the kernel linker script (`src/boot/linker.ld`), kernel code model, and static relocation.

```bash
# nightly + components + target are installed automatically from rust-toolchain.toml
cargo build              # debug
cargo build --release    # optimized: opt-level 3, LTO, single codegen unit
```

The only external dependency in the tree is `spin` (no_std, used by the filesystem layer for `Mutex`).

---

## CIWarden

Commits in this repository pass through **CIWarden**, a deterministic CI enforcement layer. The `pre-commit` hook POSTs the staged commit to a local orchestrator on `localhost:8000`, which runs seven gates sequentially:

```
lint → typecheck → security → memory → stress → test → build
```

A commit only lands if every gate passes and a merge token is issued. Start the orchestrator before committing (`python orchestrator/orchestrator.py`); bypassing with `git commit --no-verify` is discouraged.

Golem is developed under **CDMAD** — Constraint Driven Machine Assisted Development. See `GOLEM_PHASE1_BUILD_NOTES.md` for how the kernel's six subsystems were built by parallel agents under GateChain, and `GOLEM_RUNTIME_ARCHITECTURE.md` for the full runtime architecture.

---

## What Golem is not

- **Not a nanny OS.** It does not tell you what to run; it tells you what ran.
- **Not a locked-down appliance.** Everything outside the Sentinel boundary is configurable.
- **Not opinionated about your stack.** Bring your framework, model, and agent architecture. Golem governs the process, not the implementation.

---

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

---

*Copyright © 2026 Brandon Green. Licensed under the Apache 2.0 License.*
*Golem Linux is developed under the CDMAE methodology.*
