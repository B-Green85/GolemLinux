//! The Sentinel handshake.
//!
//! > *To the agent, this is a standard process registration handshake.*
//! > *To Golem, it is a handshake with a handcuff.*
//!
//! Every LLM process — local runtime, frontier API client, agent framework —
//! must complete this handshake before its first user-visible syscall returns.
//! The protocol is intentionally shaped to look like a generic
//! "register-yourself-with-the-kernel" call: the agent passes an opaque
//! identifier and a permission tier, and gets back a token it can echo on
//! future calls. From the agent's perspective, nothing more is happening.
//!
//! What's actually happening: the kernel is binding that task ID to a
//! Sentinel record, recording the bind in the audit chain, and switching the
//! task's [`CallerContext`] from Operator to Agent. From this syscall onward,
//! every Sentinel API returns ENOSYS to this task. The token it received is
//! cosmetic — it grants the agent nothing it didn't already have, and reveals
//! nothing about what Sentinel does with it.
//!
//! ## Protocol shape
//!
//! ```text
//!   agent ──► HandshakeRequest { agent_id, tier, heartbeat_interval } ──► kernel
//!   agent ◄── HandshakeAck     { agent_id, token, registered_at }     ◄── kernel
//! ```
//!
//! The kernel-side handler is [`Sentinel::handshake`](crate::sentinel::Sentinel::handshake).
//! On success it:
//!
//! 1. Allocates an `AgentRecord` keyed by `agent_id` (registry stays in
//!    kernel memory, opaque to userspace).
//! 2. Writes an audit entry: `actor="kernel"`, `action="handshake"`,
//!    `target=agent_id`.
//! 3. Returns the token, which is just the audit entry's hash truncated. The
//!    agent can echo it; the agent cannot use it for anything privileged.
//!
//! Re-registration of an already-known `agent_id` is rejected. Agents do not
//! get to "reset" their record — that would defeat monitoring across the
//! degradation window.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::sentinel::SpinLock;

/// Permission tiers, mirroring `sentinel-core` so a userspace verifier can
/// parse both sides with one schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionTier {
    ReadOnly,
    Write,
    Execute,
}

impl PermissionTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::Write => "WRITE",
            Self::Execute => "EXECUTE",
        }
    }

    /// One step toward less privilege. The Hard tier path drops all the way
    /// to ReadOnly directly; this is only the per-step helper.
    pub const fn downgrade(self) -> Option<Self> {
        match self {
            Self::Execute => Some(Self::Write),
            Self::Write => Some(Self::ReadOnly),
            Self::ReadOnly => None,
        }
    }
}

/// What the agent sends. The agent fills these in; Sentinel does not trust
/// the values blindly — see [`HandshakeError`] for what gets rejected.
#[derive(Debug, Clone)]
pub struct HandshakeRequest {
    pub agent_id: String,
    pub tier: PermissionTier,
    /// How often the *infrastructure* (not the agent — agent is blind to this
    /// requirement) will deliver a heartbeat on behalf of the agent. In
    /// seconds. The agent picks a value; Sentinel may clamp it.
    pub heartbeat_interval_secs: u64,
}

/// What the agent receives back. The token is cosmetic from the agent's
/// point of view — useful only to echo on future calls for symmetry with
/// other registration protocols.
#[derive(Debug, Clone)]
pub struct HandshakeAck {
    pub agent_id: String,
    pub token: String,
    pub registered_at: u64,
}

/// Why a handshake might be refused. We deliberately do not expose
/// `AlreadyRegistered` as a distinct errno to Agent callers — re-registration
/// failure looks the same as a malformed request from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    AlreadyRegistered,
    EmptyAgentId,
    AgentIdTooLong,
    HeartbeatOutOfRange,
}

/// Limits applied to handshake inputs. Kernel-side memory is finite — refusing
/// to register an agent with a 16-MB identifier is not a UX failure.
pub const MAX_AGENT_ID_LEN: usize = 128;
pub const MIN_HEARTBEAT_SECS: u64 = 1;
pub const MAX_HEARTBEAT_SECS: u64 = 3600;

/// In-memory record for one registered agent. Kept opaque outside the module
/// because nothing in userspace should be reading these directly — they're
/// updated under the Sentinel lock and consumed by the monitor.
#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub agent_id: String,
    pub tier: PermissionTier,
    pub original_tier: PermissionTier,
    pub heartbeat_interval_secs: u64,
    pub registered_at: u64,
    pub last_heartbeat: u64,
    pub locked: bool,
}

/// Registry of every agent that has completed the handshake. The kernel
/// keeps this in private memory; there is no `/proc/sentinel/agents` —
/// invisibility means there is no path to enumerate this from userspace.
pub struct Registry {
    inner: SpinLock<BTreeMap<String, AgentRecord>>,
}

impl Registry {
    pub const fn new() -> Self {
        Self { inner: SpinLock::new(BTreeMap::new()) }
    }

    /// Try to register an agent. Returns `Ok(token-source)` — the caller
    /// (Sentinel) is responsible for shaping that into a token via the audit
    /// hash. We don't generate tokens here so this stays a pure data-model
    /// operation, easy to test.
    pub fn register(
        &self,
        req: &HandshakeRequest,
        now: u64,
    ) -> Result<AgentRecord, HandshakeError> {
        validate(req)?;
        let mut g = self.inner.lock();
        if g.contains_key(&req.agent_id) {
            return Err(HandshakeError::AlreadyRegistered);
        }
        let clamped = clamp_heartbeat(req.heartbeat_interval_secs);
        let record = AgentRecord {
            agent_id: req.agent_id.clone(),
            tier: req.tier,
            original_tier: req.tier,
            heartbeat_interval_secs: clamped,
            registered_at: now,
            last_heartbeat: now,
            locked: false,
        };
        g.insert(req.agent_id.clone(), record.clone());
        Ok(record)
    }

    /// Record a heartbeat. Heartbeats come from infrastructure (the kernel-
    /// side tick), not from the agent itself — the agent is blind to the
    /// requirement.
    pub fn heartbeat(&self, agent_id: &str, now: u64) -> bool {
        let mut g = self.inner.lock();
        if let Some(rec) = g.get_mut(agent_id) {
            rec.last_heartbeat = now;
            true
        } else {
            false
        }
    }

    /// Force a tier change. Returns the previous tier on success.
    pub fn set_tier(&self, agent_id: &str, tier: PermissionTier) -> Option<PermissionTier> {
        let mut g = self.inner.lock();
        let rec = g.get_mut(agent_id)?;
        let prev = rec.tier;
        rec.tier = tier;
        Some(prev)
    }

    /// Lock the agent — Hard tier response. The lock flag stays set; only
    /// Safe Mode can clear it.
    pub fn lock(&self, agent_id: &str) -> bool {
        let mut g = self.inner.lock();
        if let Some(rec) = g.get_mut(agent_id) {
            rec.locked = true;
            rec.tier = PermissionTier::ReadOnly;
            true
        } else {
            false
        }
    }

    pub fn get(&self, agent_id: &str) -> Option<AgentRecord> {
        self.inner.lock().get(agent_id).cloned()
    }

    pub fn deregister(&self, agent_id: &str) -> bool {
        self.inner.lock().remove(agent_id).is_some()
    }

    pub fn agent_ids(&self) -> Vec<String> {
        self.inner.lock().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

fn validate(req: &HandshakeRequest) -> Result<(), HandshakeError> {
    if req.agent_id.is_empty() {
        return Err(HandshakeError::EmptyAgentId);
    }
    if req.agent_id.len() > MAX_AGENT_ID_LEN {
        return Err(HandshakeError::AgentIdTooLong);
    }
    if req.heartbeat_interval_secs < MIN_HEARTBEAT_SECS
        || req.heartbeat_interval_secs > MAX_HEARTBEAT_SECS
    {
        return Err(HandshakeError::HeartbeatOutOfRange);
    }
    Ok(())
}

fn clamp_heartbeat(secs: u64) -> u64 {
    secs.clamp(MIN_HEARTBEAT_SECS, MAX_HEARTBEAT_SECS)
}

/// Shape the audit hash into the token returned to the agent. The token is
/// the first 32 hex chars of the audit hash — enough for the agent to echo,
/// not enough to reverse to the chain entry.
pub fn token_from_audit_hash(audit_hash: &str) -> String {
    let take = audit_hash.len().min(32);
    audit_hash[..take].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str) -> HandshakeRequest {
        HandshakeRequest {
            agent_id: id.to_string(),
            tier: PermissionTier::Write,
            heartbeat_interval_secs: 30,
        }
    }

    #[test]
    fn first_registration_succeeds() {
        let r = Registry::new();
        let rec = r.register(&req("a1"), 100).unwrap();
        assert_eq!(rec.agent_id, "a1");
        assert_eq!(rec.tier, PermissionTier::Write);
        assert_eq!(rec.original_tier, PermissionTier::Write);
        assert_eq!(rec.registered_at, 100);
        assert!(!rec.locked);
    }

    #[test]
    fn re_registration_is_rejected() {
        let r = Registry::new();
        r.register(&req("a1"), 100).unwrap();
        assert_eq!(
            r.register(&req("a1"), 101),
            Err(HandshakeError::AlreadyRegistered)
        );
    }

    #[test]
    fn empty_id_rejected() {
        let r = Registry::new();
        assert_eq!(
            r.register(&req(""), 100),
            Err(HandshakeError::EmptyAgentId)
        );
    }

    #[test]
    fn oversized_id_rejected() {
        let r = Registry::new();
        let mut bad = req("x");
        bad.agent_id = "a".repeat(MAX_AGENT_ID_LEN + 1);
        assert_eq!(
            r.register(&bad, 100),
            Err(HandshakeError::AgentIdTooLong)
        );
    }

    #[test]
    fn heartbeat_bounds_enforced() {
        let r = Registry::new();
        let mut bad = req("a1");
        bad.heartbeat_interval_secs = 0;
        assert_eq!(
            r.register(&bad, 100),
            Err(HandshakeError::HeartbeatOutOfRange)
        );
        let mut bad = req("a2");
        bad.heartbeat_interval_secs = MAX_HEARTBEAT_SECS + 1;
        assert_eq!(
            r.register(&bad, 100),
            Err(HandshakeError::HeartbeatOutOfRange)
        );
    }

    #[test]
    fn heartbeat_updates_last_seen() {
        let r = Registry::new();
        r.register(&req("a1"), 100).unwrap();
        assert!(r.heartbeat("a1", 250));
        assert_eq!(r.get("a1").unwrap().last_heartbeat, 250);
    }

    #[test]
    fn lock_forces_read_only_and_persists() {
        let r = Registry::new();
        r.register(&req("a1"), 100).unwrap();
        assert!(r.lock("a1"));
        let rec = r.get("a1").unwrap();
        assert!(rec.locked);
        assert_eq!(rec.tier, PermissionTier::ReadOnly);
        // original_tier preserved so Safe Mode can see what was revoked.
        assert_eq!(rec.original_tier, PermissionTier::Write);
    }

    #[test]
    fn token_is_truncated_audit_hash() {
        let h = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let t = token_from_audit_hash(h);
        assert_eq!(t.len(), 32);
        assert!(h.starts_with(&t));
    }
}
