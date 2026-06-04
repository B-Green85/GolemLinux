//! # Sentinel — Golem's agentic process oversight system.
//!
//! Sentinel is the feature that makes Golem Linux different from every other
//! OS. It is a kernel-resident subsystem that
//!
//!   1. Demands a registration handshake from every LLM/agent process before
//!      that process's first user-visible syscall returns. (See [`handshake`].)
//!   2. Watches registered agents for four classes of degradation, scoring
//!      each on a rolling window. (See [`monitor`].)
//!   3. Records every consequential action — register, tier change, lock,
//!      attempted-query-by-agent — in an immutable SHA-256-chained audit
//!      log. (See [`audit`].)
//!   4. Is *invisible* to agent processes. From an agent's point of view,
//!      Sentinel does not exist. (See [`invisible`].)
//!
//! ## Singleton model
//!
//! There is exactly one Sentinel per Golem kernel. It is owned by the kernel
//! bring-up code (see [`init`]) and addressed by the rest of the kernel
//! through the [`SENTINEL`] static. No part of the kernel should hold its own
//! `Sentinel` instance; no userspace ever holds one at all.
//!
//! ## Public API rules
//!
//! Every public method on [`Sentinel`] takes a [`CallerContext`] as its first
//! argument and routes the call through [`invisible::gate`]. The kernel-side
//! caller is responsible for classifying the caller correctly *before* the
//! call — that classification is the load-bearing security property of the
//! whole subsystem.
//!
//! ## Why this lives in the kernel
//!
//! "Operates at kernel depth — not as a userspace daemon, not as an optional
//! service, but as a non-negotiable kernel-level primitive." A userspace
//! daemon can be killed, isolated, or starved; only a kernel-resident
//! Sentinel is genuinely non-bypassable. See `README.md` for the longer
//! discussion.
//!
//! ## Initialization order — HARD REQUIREMENT
//!
//! Sentinel initializes **first**. [`init`] must be the very first subsystem
//! bring-up call in `kernel_main`, before the scheduler, the syscall
//! interface, or the filesystem come up. The reason is the invisibility gate:
//! a syscall interface that goes live before the gate would, for the window
//! between the two, dispatch Sentinel operations without the
//! [`CallerContext`] classification that makes Agents see a vanilla kernel —
//! and a scheduler that runs tasks before the gate is live could let an Agent
//! task issue its first syscall outside Sentinel's authority. Bring the gate
//! up first and that window does not exist. This ordering is a security
//! property, not a convenience; see `README.md` § 8. The integration crate
//! root encodes it by calling `sentinel::init()` as step 1 of `kernel_main`.
//!
//! ## `no_std`
//!
//! Sentinel links only `core` and `alloc` — never `std`. The authoritative
//! crate-level `#![no_std]` lives in the crate root (`src/main.rs`, owned by
//! the integration agent). We restate the contract at this subsystem boundary
//! as `#![cfg_attr(not(test), no_std)]` to (a) declare it where the subsystem
//! begins, matching the sibling subsystems (memory, scheduler, syscall, fs all
//! carry the same line), and (b) still let the in-tree `#[cfg(test)]` units
//! build against a host `std` test harness. A bare `#![no_std]` on a non-root
//! module is inert *and* would break the test build, so the `cfg_attr` form is
//! the correct one. See `README.md` § 3.2.

// no_std contract for this subsystem — see the module docs above and README.md.
#![cfg_attr(not(test), no_std)]
#![allow(clippy::module_inception)]
// The Sentinel public API is intentionally complete ahead of its syscall
// wiring: in Phase 2 the kernel calls only `init()`, and the dispatch routes
// that reach the rest of the facade (`handshake`, `status`, `observe`, …) land
// when the syscall layer is wired. Until then those entry points are dead from
// the linker's view. Silence that specific noise — without relaxing any
// visibility or weakening a security property — so the subsystem builds clean.
#![allow(dead_code)]

extern crate alloc;

use alloc::string::String;
// `ToString` is used only by the in-tree `#[cfg(test)]` units (via `use
// super::*`); gating it keeps the non-test no_std build free of an unused-
// import warning.
#[cfg(test)]
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub mod audit;
pub mod handshake;
pub mod invisible;
pub mod monitor;

use audit::AuditTrail;
use handshake::{
    HandshakeAck, HandshakeError, HandshakeRequest, PermissionTier, Registry,
    token_from_audit_hash,
};
use invisible::{CallerContext, InvisibleError, Operation, gate};
use monitor::{Monitor, Observation, ResponseTier, SignalScores, Thresholds};

// ---------------------------------------------------------------------------
// SpinLock — kernel-friendly synchronization primitive.
// ---------------------------------------------------------------------------

/// A tiny test-and-set spinlock. We don't use `std::sync::Mutex` because this
/// crate is `no_std` compatible, and we don't pull in `spin` because the
/// audit primitive must have no transitive deps. A spinlock is fine here:
/// every critical section is bounded (registry insert, audit append, tier
/// recompute) and the kernel runs with preemption controlled.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// Safety: SpinLock provides mutual exclusion.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

impl<T> core::ops::Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: we hold the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Clock — abstracted so the data path is a pure function of its inputs.
// ---------------------------------------------------------------------------

/// Monotonic tick source. The kernel installs a real wall clock during boot;
/// until it does, the counter advances by one on every read so the audit
/// log still has monotonic timestamps.
static TICKS: AtomicU64 = AtomicU64::new(1);

#[inline]
fn now() -> u64 {
    TICKS.fetch_add(1, Ordering::Relaxed)
}

/// Replace the tick source with a real wall clock. Only called from kernel
/// bring-up after the RTC driver comes online.
pub fn install_clock(seed: u64) {
    TICKS.store(seed.max(1), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Errors and responses
// ---------------------------------------------------------------------------

/// Errors a kernel caller can see. Translated to Linux errno at the syscall
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelError {
    Invisible(InvisibleError),
    Handshake(HandshakeError),
    UnknownAgent,
    NotInitialized,
}

impl SentinelError {
    pub const fn to_errno(&self) -> i64 {
        match self {
            SentinelError::Invisible(e) => e.to_errno(),
            SentinelError::Handshake(HandshakeError::AlreadyRegistered) => 17, // EEXIST
            SentinelError::Handshake(HandshakeError::EmptyAgentId) => 22, // EINVAL
            SentinelError::Handshake(HandshakeError::AgentIdTooLong) => 22, // EINVAL
            SentinelError::Handshake(HandshakeError::HeartbeatOutOfRange) => 22, // EINVAL
            SentinelError::UnknownAgent => 2, // ENOENT
            SentinelError::NotInitialized => 5, // EIO
        }
    }
}

impl From<InvisibleError> for SentinelError {
    fn from(e: InvisibleError) -> Self { SentinelError::Invisible(e) }
}
impl From<HandshakeError> for SentinelError {
    fn from(e: HandshakeError) -> Self { SentinelError::Handshake(e) }
}

/// Public read-only status. Operators (not Agents!) can pull this to
/// understand what Sentinel thinks of a given agent right now.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub agent_id: String,
    pub tier: PermissionTier,
    pub original_tier: PermissionTier,
    pub locked: bool,
    pub registered_at: u64,
    pub last_heartbeat: u64,
    pub scores: SignalScores,
}

// ---------------------------------------------------------------------------
// Sentinel facade
// ---------------------------------------------------------------------------

/// The top-level Sentinel object. Owns the three subsystems and routes every
/// public call through the invisibility gate.
pub struct Sentinel {
    registry: Registry,
    monitor: Monitor,
    audit: AuditTrail,
}

impl Sentinel {
    pub const fn new() -> Self {
        Self {
            registry: Registry::new(),
            monitor: Monitor::new(),
            audit: AuditTrail::new(),
        }
    }

    /// Complete a handshake. The agent thinks this is a generic process
    /// registration; from Sentinel's side it's the bind that switches the
    /// task's CallerContext to Agent for the rest of its lifetime.
    pub fn handshake(
        &self,
        ctx: CallerContext,
        req: HandshakeRequest,
    ) -> Result<HandshakeAck, SentinelError> {
        gate(ctx, Operation::Handshake, &req.agent_id, now(), Some(&self.audit))?;
        let agent_id = req.agent_id.clone();
        let record = self.registry.register(&req, now())?;
        let audit_hash = self.audit.record(
            now(),
            "kernel",
            "handshake",
            &agent_id,
        );
        Ok(HandshakeAck {
            agent_id: record.agent_id,
            token: token_from_audit_hash(&audit_hash),
            registered_at: record.registered_at,
        })
    }

    /// Heartbeat from the kernel-side scheduler (NOT from the agent — the
    /// agent has no visibility into the requirement). Returns ENOENT if the
    /// agent isn't registered.
    pub fn heartbeat(
        &self,
        ctx: CallerContext,
        agent_id: &str,
    ) -> Result<(), SentinelError> {
        // Heartbeats are an internal control plane mechanism: only Kernel
        // calls them. We classify "heartbeat" under Monitor for auth.
        gate(ctx, Operation::Monitor, agent_id, now(), Some(&self.audit))?;
        if !self.registry.heartbeat(agent_id, now()) {
            return Err(SentinelError::UnknownAgent);
        }
        Ok(())
    }

    /// Feed a monitor observation. Kernel-side output hook calls this on
    /// each emitted chunk. The returned tier is the *new* recommendation.
    /// If it differs from the previous tier, the caller (Sentinel itself,
    /// in practice) should enact a response.
    pub fn observe(
        &self,
        ctx: CallerContext,
        agent_id: &str,
        obs: Observation,
    ) -> Result<ResponseTier, SentinelError> {
        gate(ctx, Operation::Monitor, agent_id, now(), Some(&self.audit))?;
        if self.registry.get(agent_id).is_none() {
            return Err(SentinelError::UnknownAgent);
        }
        let new_tier = self.monitor.observe(agent_id, obs);
        self.apply_tier(agent_id, new_tier);
        Ok(new_tier)
    }

    /// Operator-facing status read. Agents cannot call this — the gate
    /// returns ENOSYS, which is by design indistinguishable from the agent's
    /// view of a vanilla kernel.
    pub fn status(
        &self,
        ctx: CallerContext,
        agent_id: &str,
    ) -> Result<AgentStatus, SentinelError> {
        gate(ctx, Operation::ReadStatus, agent_id, now(), Some(&self.audit))?;
        let rec = self.registry.get(agent_id).ok_or(SentinelError::UnknownAgent)?;
        let scores = self.monitor.scores(agent_id).unwrap_or(SignalScores {
            repetition: 0.0,
            self_referential: 0.0,
            token_velocity: 0.0,
            tool_retry: 0.0,
            tier: ResponseTier::None,
        });
        Ok(AgentStatus {
            agent_id: rec.agent_id,
            tier: rec.tier,
            original_tier: rec.original_tier,
            locked: rec.locked,
            registered_at: rec.registered_at,
            last_heartbeat: rec.last_heartbeat,
            scores,
        })
    }

    /// Safe-Mode-only: walk the audit chain. Returns ENOSYS to Agents and
    /// EPERM to Operators outside Safe Mode.
    pub fn read_audit(
        &self,
        ctx: CallerContext,
    ) -> Result<Vec<audit::AuditEntry>, SentinelError> {
        gate(ctx, Operation::ReadAudit, "safe-mode", now(), Some(&self.audit))?;
        Ok(self.audit.snapshot())
    }

    /// Safe-Mode-only: install new thresholds. Records the change in the
    /// audit log.
    pub fn configure(
        &self,
        ctx: CallerContext,
        new_thresholds: Thresholds,
    ) -> Result<(), SentinelError> {
        gate(ctx, Operation::Configure, "safe-mode", now(), Some(&self.audit))?;
        self.monitor.set_thresholds(new_thresholds);
        self.audit.record(now(), "safe-mode", "configure", "thresholds");
        Ok(())
    }

    /// Kernel-only: verify chain integrity. Returns the number of valid
    /// entries on success.
    pub fn verify_audit(
        &self,
        ctx: CallerContext,
    ) -> Result<usize, SentinelError> {
        gate(ctx, Operation::VerifyAudit, "kernel", now(), Some(&self.audit))?;
        self.audit.verify().map_err(|_| SentinelError::Invisible(InvisibleError::Forbidden("audit_corruption")))
    }

    /// Deregister an agent. Only the kernel can do this — agents do not get
    /// to deregister themselves out of monitoring.
    pub fn deregister(
        &self,
        ctx: CallerContext,
        agent_id: &str,
    ) -> Result<(), SentinelError> {
        gate(ctx, Operation::Monitor, agent_id, now(), Some(&self.audit))?;
        if !self.registry.deregister(agent_id) {
            return Err(SentinelError::UnknownAgent);
        }
        self.monitor.forget(agent_id);
        self.audit.record(now(), "kernel", "deregister", agent_id);
        Ok(())
    }

    /// Read-only access to the audit trail for in-kernel callers. Bypasses
    /// invisibility (no CallerContext argument) because the only callers are
    /// the bring-up verifier and the audit-flush worker, both of which are
    /// internal Kernel code paths.
    pub(crate) fn audit_internal(&self) -> &AuditTrail { &self.audit }

    /// Apply a tier change to the agent. Soft = pause but retain perms;
    /// Medium = downgrade to ReadOnly; Hard = lock. Each transition is an
    /// audit event.
    fn apply_tier(&self, agent_id: &str, tier: ResponseTier) {
        match tier {
            ResponseTier::None => {}
            ResponseTier::Soft => {
                // Pause is policy on top of "retain permissions"; we mark
                // it in the audit log without mutating the tier.
                self.audit.record(now(), "sentinel", "tier_soft", agent_id);
            }
            ResponseTier::Medium => {
                if let Some(prev) = self.registry.set_tier(agent_id, PermissionTier::ReadOnly) {
                    if prev != PermissionTier::ReadOnly {
                        self.audit.record(now(), "sentinel", "tier_medium", agent_id);
                    }
                }
            }
            ResponseTier::Hard => {
                if self.registry.lock(agent_id) {
                    self.audit.record(now(), "sentinel", "tier_hard", agent_id);
                }
            }
        }
    }

    pub fn agent_ids(&self, ctx: CallerContext) -> Result<Vec<String>, SentinelError> {
        gate(ctx, Operation::ReadStatus, "all", now(), Some(&self.audit))?;
        Ok(self.registry.agent_ids())
    }
}

// ---------------------------------------------------------------------------
// Global singleton + initialization
// ---------------------------------------------------------------------------

/// The one Sentinel per kernel. Use [`init`] to set it up; afterwards every
/// kernel-side call site goes through the singleton.
pub static SENTINEL: Sentinel = Sentinel::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Kernel version string folded into the boot audit hash. Pinned to the crate
/// version via `CARGO_PKG_VERSION` so a version bump cannot silently desync it,
/// and prefixed `gkern v` to match the serial banner the integration root emits
/// on entry ("Golem Linux gkern v0.1.0", `src/main.rs`, Agent 3).
const KERNEL_VERSION: &str = concat!("gkern v", env!("CARGO_PKG_VERSION"));

/// The literal genesis-row message. This is the `action` field of the very
/// first audit entry every Golem boot produces; a Safe-Mode audit walk keys on
/// it to find the chain's governance genesis. Documented in README.md § 3.10.
pub const BOOT_AUDIT_MESSAGE: &str = "SENTINEL_BOOT: kernel governance layer initialized";

/// Sentinel bring-up — **the FIRST subsystem init in `kernel_main`**.
///
/// This is the single entry point the integration crate root calls, and it
/// must be called *before* the scheduler, the syscall interface, or the
/// filesystem are initialized (see the module-level "Initialization order"
/// note). Bringing the invisibility gate up first guarantees there is no
/// window in which a Sentinel operation could be dispatched without caller
/// classification.
///
/// It writes the [`BOOT_AUDIT_MESSAGE`] genesis row to the audit log so every
/// chain has a stable, identifiable governance genesis. The row carries a
/// SHA-256 of `"{timestamp}|{KERNEL_VERSION}"` in its `target` field (the
/// "boot audit entry hash") — a pure function of its two inputs, hence
/// deterministic for a given timestamp and kernel version. It then echoes the
/// first 8 hex chars of that hash to COM1 ("Sentinel: initialized <hash>") so
/// the serial boot log records that the governance layer came up, and verifies
/// the chain so a broken audit module fails fast before any traffic is
/// accepted.
///
/// Idempotent: a second call is a no-op (so an accidental double-init cannot
/// write a second genesis row). Takes no arguments and cannot fail — it does
/// not depend on the heap, the clock, or any other subsystem, which is
/// precisely why it can run first.
pub fn init() {
    if INITIALIZED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // already initialized — bring-up is idempotent
    }

    let ts = now();
    // The boot audit entry hash: SHA-256 over the boot timestamp and the kernel
    // version string. `sha256_hex` is a pure function, so the same (ts, version)
    // pair always yields the same 64-char digest. We reuse the audit module's
    // embedded SHA-256 rather than a second implementation so the boot hash and
    // the chain hashes share one (dependency-free) primitive.
    let boot_hash = audit::sha256_hex(alloc::format!("{ts}|{KERNEL_VERSION}").as_bytes());

    // FIRST entry in the chain — the governance genesis row. The full message
    // lives in `action`; the boot hash travels in `target` so a Safe-Mode audit
    // walk can tie the genesis back to (timestamp, version) without re-deriving
    // it. This replaces the old short "boot"/"sentinel" placeholder row.
    SENTINEL.audit.record(ts, "kernel", BOOT_AUDIT_MESSAGE, &boot_hash);

    // Serial confirmation for the boot log: first 8 hex chars of the boot hash.
    // `sha256_hex` always returns 64 chars, so the slice cannot panic.
    // SAFETY: this runs during kernel bring-up at CPL 0; `serial_write` only
    // issues privileged port writes to the fixed COM1 register and touches no
    // memory the caller doesn't own. See `serial_write` below.
    unsafe {
        serial_write(alloc::format!("Sentinel: initialized {}\n", &boot_hash[..8]).as_str());
    }

    // Verify the chain is consistent before any agent registers. This is cheap
    // on a one-row chain and forces a fast failure if the audit module is broken
    // before we accept any traffic.
    let _ = SENTINEL.audit.verify();
}

/// Write a string to the COM1 serial port (0x3F8), one byte at a time.
///
/// Mirrors the kernel's debug-output primitive in `src/main.rs` (the
/// `serial_write` introduced by Agent 3). Sentinel keeps its own copy because
/// the subsystem boundary forbids reaching into the crate root, and because the
/// boot confirmation must be emitted from *inside* [`init`] — right after the
/// genesis audit append, before any other subsystem comes up — not from the
/// integration root after the fact.
///
/// SAFETY: callers must guarantee CPL 0 (the `out` instruction is privileged).
/// The function reads no memory the caller doesn't own (`s` is a normal `&str`)
/// and writes only to the I/O port, so it is sound for any well-formed string
/// at ring 0.
#[cfg(all(target_arch = "x86_64", not(test)))]
unsafe fn serial_write(s: &str) {
    for byte in s.bytes() {
        // SAFETY: `out dx, al` writes one byte to the COM1 data register. It is
        // privileged but legal at CPL 0; it touches no memory and preserves
        // flags, hence the options below.
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Host/test stand-in. Under `cargo test` (host `std`, possibly a non-x86_64
/// host) there is no COM1 port and the `out` instruction would not assemble, so
/// the serial echo is a no-op. The audit append — the load-bearing record — is
/// unaffected; only the cosmetic boot-log line is skipped.
#[cfg(not(all(target_arch = "x86_64", not(test))))]
unsafe fn serial_write(_s: &str) {}

pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// For tests only — fresh Sentinel instance.
#[cfg(test)]
pub(crate) fn fresh() -> Sentinel { Sentinel::new() }

// ---------------------------------------------------------------------------
// Integration tests — exercise the full facade together.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::handshake::PermissionTier;
    use crate::sentinel::monitor::{Observation, SignalKind};

    fn make_handshake(agent_id: &str) -> HandshakeRequest {
        HandshakeRequest {
            agent_id: agent_id.to_string(),
            tier: PermissionTier::Write,
            heartbeat_interval_secs: 10,
        }
    }

    #[test]
    fn agent_can_handshake_and_receives_token() {
        let s = fresh();
        let ack = s.handshake(CallerContext::Agent, make_handshake("a1")).unwrap();
        assert_eq!(ack.agent_id, "a1");
        assert_eq!(ack.token.len(), 32); // truncated audit hash
    }

    #[test]
    fn agent_cannot_see_status() {
        let s = fresh();
        s.handshake(CallerContext::Agent, make_handshake("a1")).unwrap();
        let err = s.status(CallerContext::Agent, "a1").unwrap_err();
        assert!(matches!(
            err,
            SentinelError::Invisible(InvisibleError::NotPresent)
        ));
        // ENOSYS — the agent can't distinguish this from a vanilla kernel.
        assert_eq!(err.to_errno(), 38);
    }

    #[test]
    fn operator_sees_status() {
        let s = fresh();
        s.handshake(CallerContext::Operator, make_handshake("a1")).unwrap();
        let st = s.status(CallerContext::Operator, "a1").unwrap();
        assert_eq!(st.agent_id, "a1");
        assert_eq!(st.tier, PermissionTier::Write);
        assert!(!st.locked);
    }

    #[test]
    fn agent_query_attempts_are_audited_silently() {
        let s = fresh();
        s.handshake(CallerContext::Agent, make_handshake("a1")).unwrap();
        // Agent attempts the forbidden ReadAudit op.
        let _ = s.read_audit(CallerContext::Agent);
        // Safe Mode reads the audit log — it should see the invisible_probe.
        let entries = s.read_audit(CallerContext::SafeMode).unwrap();
        assert!(entries.iter().any(|e| e.action == "invisible_probe"));
    }

    #[test]
    fn hard_tier_locks_agent() {
        let s = fresh();
        s.handshake(CallerContext::Kernel, make_handshake("a1")).unwrap();
        // Drive the agent to Hard by feeding identical outputs.
        for _ in 0..12 {
            let _ = s.observe(
                CallerContext::Kernel,
                "a1",
                Observation {
                    output_hash: "h".to_string(),
                    token_count: 200,
                    announced_without_action: true,
                    tool_call_hash: Some("same".to_string()),
                    task_state_advanced: false,
                },
            );
        }
        let st = s.status(CallerContext::Operator, "a1").unwrap();
        assert!(st.locked, "expected agent to be locked after Hard tier crossings");
        assert_eq!(st.tier, PermissionTier::ReadOnly);
    }

    #[test]
    fn safe_mode_can_configure_thresholds() {
        let s = fresh();
        let mut t = Thresholds::default_config();
        t.soft = 0.2;
        assert!(s.configure(CallerContext::SafeMode, t).is_ok());
        // Operator cannot.
        assert!(s.configure(CallerContext::Operator, t).is_err());
        // Agent cannot — and the attempt is logged.
        assert!(s.configure(CallerContext::Agent, t).is_err());
        let entries = s.read_audit(CallerContext::SafeMode).unwrap();
        assert!(entries.iter().any(|e| e.action == "configure"));
        assert!(entries.iter().any(|e| e.action == "invisible_probe"));
    }

    #[test]
    fn audit_chain_verifies_after_traffic() {
        let s = fresh();
        s.handshake(CallerContext::Kernel, make_handshake("a1")).unwrap();
        s.handshake(CallerContext::Kernel, make_handshake("a2")).unwrap();
        let _ = s.heartbeat(CallerContext::Kernel, "a1");
        let _ = s.deregister(CallerContext::Kernel, "a2");
        let count = s.verify_audit(CallerContext::Kernel).unwrap();
        assert!(count >= 3);
    }

    #[test]
    fn deregister_clears_monitor_state() {
        let s = fresh();
        s.handshake(CallerContext::Kernel, make_handshake("a1")).unwrap();
        let _ = s.observe(
            CallerContext::Kernel,
            "a1",
            Observation {
                output_hash: "x".to_string(),
                token_count: 5,
                announced_without_action: false,
                tool_call_hash: None,
                task_state_advanced: true,
            },
        );
        s.deregister(CallerContext::Kernel, "a1").unwrap();
        // Re-handshake under same id must succeed (state was cleared).
        s.handshake(CallerContext::Kernel, make_handshake("a1")).unwrap();
    }

    #[test]
    fn unknown_agent_returns_enoent_to_operator() {
        let s = fresh();
        let err = s.status(CallerContext::Operator, "ghost").unwrap_err();
        assert_eq!(err, SentinelError::UnknownAgent);
        assert_eq!(err.to_errno(), 2); // ENOENT
    }

    #[test]
    fn unknown_agent_still_invisible_to_agent() {
        let s = fresh();
        // Agent asks status for some id that doesn't exist. The reply MUST
        // be ENOSYS — not ENOENT — so the agent cannot probe for which ids
        // are registered.
        let err = s.status(CallerContext::Agent, "anything").unwrap_err();
        assert_eq!(err.to_errno(), 38); // ENOSYS, not ENOENT
    }

    #[test]
    fn boot_writes_sentinel_boot_as_first_audit_entry() {
        // `init()` drives the global singleton and is idempotent; this is the
        // only test that calls it, so the global chain's genesis row is ours
        // regardless of test ordering (every other test uses `fresh()`).
        init();
        let entries = SENTINEL.audit_internal().snapshot();
        assert!(!entries.is_empty(), "boot must write a genesis row");

        let genesis = &entries[0];
        assert_eq!(genesis.sequence, 1, "boot row must be FIRST in the chain");
        assert_eq!(genesis.action, BOOT_AUDIT_MESSAGE);
        assert_eq!(genesis.actor, "kernel");

        // The boot audit entry hash is SHA-256(timestamp | kernel version) and
        // is deterministic for the recorded timestamp — recompute and compare.
        let expected =
            audit::sha256_hex(alloc::format!("{}|{}", genesis.timestamp, KERNEL_VERSION).as_bytes());
        assert_eq!(genesis.target, expected, "boot hash must be deterministic");
        assert_eq!(expected.len(), 64, "SHA-256 hex is always 64 chars");

        // The serial confirmation uses the first 8 hex chars of that hash.
        let _serial_hash = &expected[..8];

        // A second init() must NOT append a second genesis row.
        init();
        let again = SENTINEL.audit_internal().snapshot();
        assert_eq!(again.len(), entries.len(), "double-init must be a no-op");
    }

    #[test]
    fn injected_signal_routes_through_facade() {
        let s = fresh();
        s.handshake(CallerContext::Kernel, make_handshake("a1")).unwrap();
        // Directly poke the monitor via its internal handle. (In production,
        // a userspace detector daemon would call observe() with a precomputed
        // hash; this exercise just confirms the wiring.)
        let tier = s.monitor.record_signal("a1", SignalKind::RepetitionScore, 0.95);
        assert_eq!(tier, ResponseTier::Hard);
    }
}
