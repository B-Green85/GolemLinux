//! The invisibility boundary.
//!
//! Sentinel is not just *protected* from agent processes — it is *not present*
//! from their point of view. An agent that asks "is there a Sentinel here?"
//! must receive the same answer it would get on a vanilla kernel with no
//! Sentinel installed: nothing. ENOSYS. No such subsystem.
//!
//! This file is the single chokepoint that enforces that property. Every
//! public Sentinel API funnels through [`gate`], which classifies the caller
//! and either lets the call proceed (Operator / Kernel / SafeMode contexts)
//! or returns [`InvisibleError`] (Agent context).
//!
//! ## Why this is its own module
//!
//! Invisibility is the architectural commitment that distinguishes Golem from
//! every other OS. If we scatter the check across handshake / monitor / audit,
//! it becomes a policy spread across files — and the moment one path forgets
//! the check, the property is broken silently. Concentrating the gate here
//! means every caller does the same check the same way, and a future code
//! reviewer can audit invisibility by reading one file.
//!
//! ## What invisibility is NOT
//!
//! It is not obfuscation. It is not "the symbol table doesn't include
//! `sentinel_*`". It is not "we filter the entry from `/proc`". Those are
//! cosmetic. The real boundary is: from an Agent-classified syscall context,
//! the Sentinel syscall numbers return `-ENOSYS` exactly the way an
//! unimplemented syscall would; the dispatch table does not branch, does not
//! delay, does not log differently. The agent observes a vanilla kernel.
//!
//! The *only* exception — and it is a load-bearing one — is that the act of
//! an Agent attempting to call a Sentinel API is itself recorded to the audit
//! log, asynchronously, *before* ENOSYS is returned. The agent cannot tell
//! this happened; the operator can read about it in Safe Mode.
//!
//! ## The Safe Mode / main OS split (compile-time, not runtime)
//!
//! Sentinel's configuration interface — reading the audit chain and mutating
//! thresholds — exists for exactly one reason: the agent-free Safe Mode
//! recovery environment needs it. In the main OS that interface must not exist
//! at all. Crucially, "must not exist" is enforced at **compile time** by the
//! [`SAFE_MODE_ACTIVE`] constant, which is wired to the `sentinel-safemode`
//! Cargo feature. The main OS kernel (`gkern`) is built without the feature and
//! cannot be talked into exposing config by any runtime input; the Safe Mode
//! kernel (`gkern-safemode`) is built with it. This mirrors the
//! `sentinel-enforcer` / `sentinel-observer` split in the v3 spec: the
//! capability is not merely refused in the wrong build — it is *not compiled
//! into* the wrong build.

extern crate alloc;
use alloc::string::String;
use alloc::string::ToString;

use crate::sentinel::audit::AuditTrail;

/// Compile-time selector for whether this binary is the Safe Mode recovery
/// kernel (`gkern-safemode`) or the main OS kernel (`gkern`).
///
/// This is the single source of truth for the Safe-Mode / main-OS split, and
/// it is **deliberately a `const`, never a `static` or a runtime variable**:
/// there is no code path — from userspace, from an Agent, from an Operator, or
/// even from Kernel context — that can flip it. Its value is fixed when the
/// binary is linked.
///
/// * `gkern-safemode` is built with `cargo build --features sentinel-safemode`
///   and sees `true` → the Sentinel configuration interface is present and
///   reachable from the Safe Mode serial terminal.
/// * `gkern` (the main OS) is built without the feature and sees `false` → the
///   configuration interface does not exist; every config query is denied just
///   as it would be on a kernel with no Sentinel config subsystem compiled in.
///
/// Because it is `const`, the dead branch in [`is_authorized`] is eliminated by
/// the compiler: in the `gkern` build there is literally no machine code that
/// would authorize a configuration operation.
#[cfg(feature = "sentinel-safemode")]
pub const SAFE_MODE_ACTIVE: bool = true;

/// See the `cfg(feature = "sentinel-safemode")` variant above. In the main OS
/// build the Sentinel configuration surface does not exist, so this is `false`.
#[cfg(not(feature = "sentinel-safemode"))]
pub const SAFE_MODE_ACTIVE: bool = false;

/// Classification of the caller of a Sentinel API. Kernel-side code assigns
/// this when crossing the syscall boundary; the classification is a property
/// of the *task* doing the call, not of the call site.
///
/// The classification is sticky for the lifetime of the task. A process
/// registered as an Agent stays an Agent until it exits — no escape via fork,
/// execve, or setuid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerContext {
    /// A process that has completed (or is in the middle of) a Sentinel
    /// handshake. Sees a vanilla kernel from Sentinel's surface.
    Agent,
    /// A normal, non-LLM user process. Can read public status but cannot
    /// configure Sentinel.
    Operator,
    /// The Safe Mode recovery environment. The only context in which Sentinel
    /// configuration is mutable.
    SafeMode,
    /// Internal kernel call paths — bring-up, scheduler tick, audit flush.
    Kernel,
}

/// Operations the gate authorizes. Sentinel exposes a tiny surface; listing
/// it as an enum forces every new operation to add a row to [`is_authorized`]
/// rather than slipping in unchecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Complete a handshake. The one operation an Agent may invoke (and even
    /// then, the Agent's view of the API is by design indistinguishable from
    /// a generic "register process" syscall).
    Handshake,
    /// Record a passive monitoring observation. Internal — Kernel only.
    Monitor,
    /// Read public agent status (does this agent exist, is it locked).
    /// Operators can ask; Agents cannot.
    ReadStatus,
    /// Inspect the audit chain. Only Safe Mode.
    ReadAudit,
    /// Mutate Sentinel configuration. Only Safe Mode.
    Configure,
    /// Verify the audit chain integrity. Kernel + SafeMode.
    VerifyAudit,
}

/// What you get back when you ask Sentinel something it will not tell you.
///
/// The variants intentionally do not differentiate "not authorized" from
/// "does not exist" when the caller is an Agent. The Agent-facing wire
/// representation is always ENOSYS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvisibleError {
    /// Agent context asked about Sentinel. Marshalled to ENOSYS at the
    /// syscall boundary so the agent sees a vanilla kernel.
    NotPresent,
    /// Operator-level caller asked for an Operator-forbidden operation
    /// (e.g. mutating config from anything other than Safe Mode).
    Forbidden(&'static str),
}

impl InvisibleError {
    /// Map to the Linux errno convention used by the syscall layer. We
    /// deliberately use the same errno for `NotPresent` as for an unwired
    /// syscall slot: that's the invisibility contract.
    pub const fn to_errno(&self) -> i64 {
        match self {
            InvisibleError::NotPresent => 38, // ENOSYS
            InvisibleError::Forbidden(_) => 1, // EPERM
        }
    }
}

/// The gate. Every Sentinel public API goes through this exactly once, at
/// entry. Returns `Ok(())` if the operation is authorized, otherwise the
/// appropriate `InvisibleError`.
///
/// `audit` is `Some` whenever the call is initiated from outside the kernel;
/// the gate records denied Agent calls so the operator can see them later.
/// Internal Kernel calls pass `None` (we don't want bring-up to drag the
/// audit lock onto every check).
pub fn gate(
    ctx: CallerContext,
    op: Operation,
    actor_id: &str,
    timestamp: u64,
    audit: Option<&AuditTrail>,
) -> Result<(), InvisibleError> {
    if is_authorized(ctx, op) {
        return Ok(());
    }

    // Denied. If the denial is because an Agent asked, log it silently and
    // return ENOSYS. The Agent cannot tell the audit entry happened.
    if matches!(ctx, CallerContext::Agent) {
        if let Some(a) = audit {
            a.record(
                timestamp,
                actor_id,
                "invisible_probe",
                op_name(op),
            );
        }
        return Err(InvisibleError::NotPresent);
    }

    Err(InvisibleError::Forbidden(op_name(op)))
}

/// Authorization matrix. Read top-to-bottom for each operation; the matrix
/// is small enough to verify by eye.
const fn is_authorized(ctx: CallerContext, op: Operation) -> bool {
    use CallerContext::*;
    use Operation::*;
    match (ctx, op) {
        // Handshake: an Agent process is *expected* to call this — that's the
        // whole point of the protocol. The Agent invokes a syscall it thinks
        // is generic process-registration; Sentinel is on the other end.
        (Agent, Handshake) => true,
        (Operator, Handshake) => true,
        (SafeMode, Handshake) => false, // Safe Mode is agent-free, by definition.
        (Kernel, Handshake) => true,

        // Monitor: passive hook driven by the kernel itself.
        (Kernel, Monitor) => true,
        (_, Monitor) => false,

        // ReadStatus: operators can poll; agents cannot see Sentinel exists.
        (Operator, ReadStatus) => true,
        (Kernel, ReadStatus) => true,
        (SafeMode, ReadStatus) => true,
        (Agent, ReadStatus) => false,

        // ReadAudit + Configure: the Sentinel *configuration interface*. This
        // is the one surface split at COMPILE TIME rather than at runtime.
        //
        // In the main OS build (`gkern`, no `sentinel-safemode` feature)
        // `SAFE_MODE_ACTIVE` is `false`, so even a SafeMode-classified caller is
        // unauthorized — the interface is not present. Only the Safe Mode
        // recovery kernel (`gkern-safemode`) compiles this capability in, where
        // `SAFE_MODE_ACTIVE` is `true` and the serial terminal can read the
        // audit chain and reconfigure Sentinel.
        //
        // This is the architectural commitment that "Sentinel can be
        // configured" is true only in the agent-free recovery environment —
        // and now it is true only in the *binary* built for that environment.
        (SafeMode, ReadAudit) => SAFE_MODE_ACTIVE,
        (_, ReadAudit) => false,
        (SafeMode, Configure) => SAFE_MODE_ACTIVE,
        (_, Configure) => false,

        // VerifyAudit: chain integrity check. Kernel runs it on boot; Safe
        // Mode runs it on demand. Nothing else.
        (Kernel, VerifyAudit) => true,
        (SafeMode, VerifyAudit) => true,
        (_, VerifyAudit) => false,
    }
}

const fn op_name(op: Operation) -> &'static str {
    match op {
        Operation::Handshake => "handshake",
        Operation::Monitor => "monitor",
        Operation::ReadStatus => "read_status",
        Operation::ReadAudit => "read_audit",
        Operation::Configure => "configure",
        Operation::VerifyAudit => "verify_audit",
    }
}

/// Convenience: format a denial reason for inclusion in operator-facing
/// errors. Never expose this string to Agent callers.
pub fn describe_error(e: InvisibleError) -> String {
    match e {
        InvisibleError::NotPresent => "ENOSYS".to_string(),
        InvisibleError::Forbidden(op) => {
            let mut s = String::from("EPERM: ");
            s.push_str(op);
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_cannot_see_sentinel_status() {
        let audit = AuditTrail::new();
        let res = gate(
            CallerContext::Agent,
            Operation::ReadStatus,
            "agent-7",
            100,
            Some(&audit),
        );
        assert_eq!(res, Err(InvisibleError::NotPresent));
        // The probe MUST be recorded silently.
        assert_eq!(audit.len(), 1);
        let snap = audit.snapshot();
        assert_eq!(snap[0].action, "invisible_probe");
        assert_eq!(snap[0].target, "read_status");
    }

    #[test]
    fn agent_can_handshake() {
        let audit = AuditTrail::new();
        assert!(gate(
            CallerContext::Agent,
            Operation::Handshake,
            "agent-7",
            100,
            Some(&audit),
        ).is_ok());
        // Handshake auth path must NOT generate an invisible_probe entry.
        assert!(audit.is_empty());
    }

    #[test]
    fn operator_cannot_configure_outside_safe_mode() {
        let audit = AuditTrail::new();
        let res = gate(
            CallerContext::Operator,
            Operation::Configure,
            "op-1",
            100,
            Some(&audit),
        );
        assert_eq!(res, Err(InvisibleError::Forbidden("configure")));
        // Operator denials do NOT generate invisible_probe entries — they're
        // visible failures, not silent ones.
        assert!(audit.is_empty());
    }

    #[test]
    fn errno_invisibility_collision_is_intentional() {
        // The whole invisibility contract depends on NotPresent → ENOSYS
        // being indistinguishable from an unimplemented syscall. Lock the
        // value here so a future refactor can't accidentally split them.
        assert_eq!(InvisibleError::NotPresent.to_errno(), 38);
    }

    // --- Compile-time Safe Mode / main OS split --------------------------
    //
    // The following tests pin the behavior of the `sentinel-safemode` feature.
    // The invisibility tests above hold in BOTH builds; these assert the
    // config-interface split on top of that.

    #[test]
    fn agent_config_probe_is_always_invisible() {
        // Invisibility is unconditional and entirely independent of the
        // safe-mode feature: an Agent probing the configuration interface sees
        // ENOSYS in every build, and the probe is recorded silently. This is
        // the property that must NEVER be weakened — assert it with the feature
        // on or off.
        for op in [Operation::Configure, Operation::ReadAudit] {
            let audit = AuditTrail::new();
            let res = gate(CallerContext::Agent, op, "agent-9", 100, Some(&audit));
            assert_eq!(res, Err(InvisibleError::NotPresent));
            assert_eq!(audit.len(), 1);
            assert_eq!(audit.snapshot()[0].action, "invisible_probe");
        }
    }

    #[cfg(not(feature = "sentinel-safemode"))]
    #[test]
    fn main_os_build_has_no_config_interface() {
        // This is the `gkern` (main OS) build. The configuration interface is
        // compiled out: `SAFE_MODE_ACTIVE` is false and not even a SafeMode
        // caller can read the audit chain or reconfigure Sentinel.
        assert!(!SAFE_MODE_ACTIVE);

        let audit = AuditTrail::new();
        assert!(gate(
            CallerContext::SafeMode,
            Operation::Configure,
            "operator",
            100,
            Some(&audit),
        )
        .is_err());
        assert!(gate(
            CallerContext::SafeMode,
            Operation::ReadAudit,
            "operator",
            100,
            Some(&audit),
        )
        .is_err());
        // These are not Agent denials, so nothing is recorded as a probe.
        assert!(audit.is_empty());
    }

    #[cfg(feature = "sentinel-safemode")]
    #[test]
    fn safe_mode_build_exposes_config_interface() {
        // This is the `gkern-safemode` build. The configuration interface is
        // present: `SAFE_MODE_ACTIVE` is true and the SafeMode serial terminal
        // can both read the audit chain and reconfigure Sentinel.
        assert!(SAFE_MODE_ACTIVE);

        let audit = AuditTrail::new();
        assert!(gate(
            CallerContext::SafeMode,
            Operation::Configure,
            "operator",
            100,
            Some(&audit),
        )
        .is_ok());
        assert!(gate(
            CallerContext::SafeMode,
            Operation::ReadAudit,
            "operator",
            100,
            Some(&audit),
        )
        .is_ok());
    }

    #[cfg(feature = "sentinel-safemode")]
    #[test]
    fn safe_mode_can_configure() {
        let audit = AuditTrail::new();
        assert!(gate(
            CallerContext::SafeMode,
            Operation::Configure,
            "operator",
            100,
            Some(&audit),
        ).is_ok());
    }
}
