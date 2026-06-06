//! Sentinel boot-time configuration **migration daemon**.
//!
//! Sentinel cannot be reconfigured from the running OS — the only context
//! permitted to change its tuning knobs is Safe Mode (`README.md` § 3.9). Safe
//! Mode is an offline, single-operator serial session: it edits the thresholds,
//! seals them into a fixed-address **migration buffer** (owned by
//! [`crate::safemode::sentinel_config`], Agent 2), and exits. The *next* main-OS
//! boot must pick that buffer up and apply it before any agent can run. That is
//! this module's entire job.
//!
//! ## What [`run`] does, in order
//!
//! 1. Reads the [`MigrationBuffer`] from its fixed address (Agent 2's
//!    [`read_migration_buffer`]).
//! 2. Checks the **magic header**. A cold boot leaves garbage / zeroes at the
//!    address; only a Safe Mode seal writes [`MIGRATION_MAGIC`]. No magic ⇒
//!    nothing was staged ⇒ `Ok(())`, boot proceeds untouched.
//! 3. For a buffer that *claims* to be a Safe Mode seal (magic matches), it is
//!    authenticated: the layout **version** must be understood and the
//!    **SHA-256 checksum** must recompute. A claim that fails authentication is
//!    a migration failure (corruption or tampering) — [`run`] returns `Err` and
//!    the caller must halt.
//! 4. Checks the **dirty flag**. An authenticated-but-clean buffer (`dirty ==
//!    false`) means "nothing changed since last session" ⇒ `Ok(())`.
//! 5. A dirty, authenticated buffer is applied to the live [`Monitor`] through
//!    the **internal Sentinel config API** ([`Monitor::set_thresholds`]) — not a
//!    socket, not IPC; this is a kernel subsystem talking to itself. The apply
//!    is then **confirmed** by reading the thresholds back.
//! 6. A migration audit entry is appended to the Sentinel [`AUDIT`] chain,
//!    carrying the config's SHA-256 checksum. (`AuditTrail::record` additionally
//!    chains the entry under its own SHA-256, so the migration is doubly
//!    hashed.)
//! 7. The **dirty flag is cleared** and the buffer re-sealed, so the next boot
//!    sees an authenticated-but-clean buffer and does not re-apply. Re-sealing
//!    is required: clearing `dirty` without recomputing the checksum would fail
//!    step 3 on the next boot and *brick* init.
//!
//! ## Failure blocks init — deliberately
//!
//! An unresolved Sentinel configuration at boot is **not** a recoverable
//! condition: it would mean booting the agent fleet under a governance config we
//! could not verify. So every authenticated-but-bad or failed-to-apply outcome
//! returns `Err`. The integration root maps that `Err` to a halt (see the init
//! order in [`crate::sentinel`]). We never "skip" a failed migration.
//!
//! ## Where this runs in the boot sequence
//!
//! See [`run`] for the precise placement and the heap/mapping reasons it must
//! run **after `memory::init`** and before the scheduler.
//!
//! `no_std` + `alloc`: this module allocates only through the same `alloc`
//! surface the rest of the kernel uses (checksum verification and audit append).
//! There is no `std` here.

extern crate alloc;

use alloc::format;
use alloc::string::String;

use crate::safemode::sentinel_config::{
    read_migration_buffer, MigrationBuffer, MIGRATION_MAGIC, MIGRATION_VERSION,
};
use crate::sentinel::monitor::Thresholds;
use crate::sentinel::{AUDIT, MONITOR};

/// Audit `actor` for every migration entry. Matches the `"safe-mode"` actor the
/// architecture assigns to configuration events (`README.md` § 3.9): the change
/// originated in a Safe Mode session even though the kernel replays it at boot.
const ACTOR: &str = "safe-mode";

/// Audit `action` for a successfully applied migration.
const ACTION_APPLY: &str = "migration_apply";
/// Audit `action` for an authenticated buffer that requested no change.
const ACTION_NOOP: &str = "migration_noop";
/// Audit `action` for a buffer that failed authentication or apply.
const ACTION_REJECT: &str = "migration_reject";

/// Why a migration could not be completed. Every variant is a boot-halting
/// condition — the integration root must not continue init when [`run`] returns
/// one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationError {
    /// The buffer carries [`MIGRATION_MAGIC`] but a layout version this kernel
    /// does not understand. Refusing to guess at a foreign layout is safer than
    /// misinterpreting threshold bytes.
    UnsupportedVersion { found: u32, expected: u32 },
    /// The buffer's stored SHA-256 checksum does not match a recomputation over
    /// its contents — corruption or tampering. The staged config is untrusted.
    ChecksumMismatch,
    /// The thresholds were pushed to the monitor but reading them back did not
    /// match what we wrote — the configuration did not take effect.
    ApplyFailed,
}

impl MigrationError {
    /// A short, static description suitable for the boot serial log.
    pub fn reason(self) -> &'static str {
        match self {
            MigrationError::UnsupportedVersion { .. } => {
                "migration: unsupported migration-buffer version"
            }
            MigrationError::ChecksumMismatch => {
                "migration: SHA-256 checksum verification failed"
            }
            MigrationError::ApplyFailed => "migration: applied config did not take effect",
        }
    }
}

impl core::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.reason())
    }
}

/// Run the Sentinel configuration migration. Returns `Ok(())` when there is
/// nothing to do or a staged change has been applied and audited; returns `Err`
/// when an authenticated change could not be verified or applied.
///
/// # Boot placement (read before wiring `kernel_main`)
///
/// `run()` must be called **after `sentinel::init()` and after `memory::init()`,
/// and before `scheduler::init()`**:
///
/// * *After `sentinel::init()`* — the Sentinel facade ([`AUDIT`], [`MONITOR`])
///   must exist before we apply to it.
/// * *After `memory::init()`* — `run()` both **allocates** (SHA-256 checksum
///   verification and the audit append) and **dereferences the migration page**
///   at `safemode::sentinel_config::MIGRATION_BUFFER_ADDR`. The kernel heap is
///   only online once `memory::init` has run `heap::init` (see
///   `memory::init_from_info`), and the physical migration page is only mapped
///   after paging is up. The original Phase-5 spec diagram placed migration
///   *before* `memory::init`; that ordering would fault on the first allocation
///   and on the unmapped read. The hard requirement from the spec — "after
///   `sentinel::init()`, before the scheduler starts" — is preserved.
/// * *Before `scheduler::init()`* — no agent task may run under a Sentinel
///   configuration we have not finished resolving.
///
/// The caller must **halt on `Err`** (init must not proceed):
///
/// ```ignore
/// sentinel::init();
/// memory::init(memory_map)?;          // heap + mapping online
/// if let Err(e) = sentinel::migration::run() {
///     serial_write(e.reason());        // optional: report to COM1
///     halt();                          // unresolved Sentinel config — do NOT continue
/// }
/// scheduler::init()?;
/// ```
///
/// # Safety / preconditions
///
/// Reads and (on a clean-up write) writes the fixed migration page. This is
/// sound under the same contract Safe Mode wrote it under: at this point in boot
/// we are single-threaded, pre-scheduler, the sole accessor of that page, and
/// the page is mapped (identity-mapped low physical RAM, the assumption the
/// `safemode::sentinel_config` and `ipc` modules already document).
pub fn run() -> Result<(), MigrationError> {
    // SAFETY: MIGRATION_BUFFER_ADDR is the reserved migration page documented by
    // `safemode::sentinel_config`. Per the boot placement above, paging is up so
    // the page is readable; the load is volatile because Safe Mode wrote it in a
    // previous boot session and the value is not derived from program order.
    let buf = unsafe { read_migration_buffer() };

    match decide(&buf) {
        // No Safe Mode seal present (cold boot / never configured). Nothing to
        // migrate and nothing worth auditing — boot proceeds untouched.
        Decision::NoBuffer => Ok(()),

        // Authenticated, but the operator changed nothing. Record the clean
        // pass for forensics, then proceed.
        Decision::NoChange => {
            AUDIT.record(buf.timestamp, ACTOR, ACTION_NOOP, &checksum_hex(&buf));
            Ok(())
        }

        // Authenticated change staged: apply it through the internal config API.
        Decision::Apply(desired) => {
            MONITOR.set_thresholds(desired);

            // Confirm the apply actually took before we trust it.
            if !thresholds_eq(&MONITOR.thresholds(), &desired) {
                audit_reject(&buf, "apply");
                return Err(MigrationError::ApplyFailed);
            }

            // Audit the applied migration, carrying the config's SHA-256.
            AUDIT.record(buf.timestamp, ACTOR, ACTION_APPLY, &checksum_hex(&buf));

            // Clear the dirty flag so the next boot does not re-apply, and
            // re-seal so the buffer stays authenticatable (clearing without
            // re-seal would fail the checksum check on the next boot).
            let mut cleared = buf;
            cleared.dirty = false;
            cleared.seal();
            // SAFETY: same reserved migration page; we are the sole writer at
            // this point in boot (pre-scheduler, single-threaded), so the
            // volatile store cannot be observed torn by any other context.
            unsafe { cleared.write_to_fixed_address() };

            Ok(())
        }

        // Authenticated claim that failed verification — halt-worthy.
        Decision::Reject(err, why) => {
            audit_reject(&buf, why);
            Err(err)
        }
    }
}

/// The decision [`run`] derives from a buffer, separated out so the policy is a
/// pure function of its input — no hardware, no globals — and is unit-testable.
enum Decision {
    /// No Safe Mode seal at the address.
    NoBuffer,
    /// Authenticated and clean: nothing to apply.
    NoChange,
    /// Authenticated and dirty: apply these thresholds.
    Apply(Thresholds),
    /// Authenticated claim that failed verification. Carries the error to
    /// surface and a short reason tag for the audit entry.
    Reject(MigrationError, &'static str),
}

/// Classify a buffer. Pure: no I/O, no global state.
///
/// Order matters. The magic is checked first so uninitialized DRAM (whose bytes
/// could happen to set the `dirty` byte) can never be mistaken for a staged
/// change. Only once the magic identifies this as a Safe Mode write do we treat
/// version/checksum failures as *errors* rather than "no buffer".
fn decide(buf: &MigrationBuffer) -> Decision {
    if buf.magic != MIGRATION_MAGIC {
        return Decision::NoBuffer;
    }
    if buf.version != MIGRATION_VERSION {
        return Decision::Reject(
            MigrationError::UnsupportedVersion {
                found: buf.version,
                expected: MIGRATION_VERSION,
            },
            "version",
        );
    }
    // Recompute the SHA-256 over the buffer's canonical serialization and
    // compare against the stored checksum (Agent 2's `compute_checksum`).
    if buf.compute_checksum() != buf.checksum {
        return Decision::Reject(MigrationError::ChecksumMismatch, "checksum");
    }
    if !buf.dirty {
        return Decision::NoChange;
    }
    Decision::Apply(buf.to_thresholds())
}

/// Append a rejection entry to the audit chain, tagged with the failure reason
/// and the buffer's checksum. Best-effort forensic record before [`run`] halts.
fn audit_reject(buf: &MigrationBuffer, why: &str) {
    let target = format!("{why} checksum={}", checksum_hex(buf));
    AUDIT.record(buf.timestamp, ACTOR, ACTION_REJECT, &target);
}

/// Render the buffer's 32-byte SHA-256 checksum as 64 lowercase hex chars.
fn checksum_hex(buf: &MigrationBuffer) -> String {
    let mut s = String::with_capacity(64);
    for b in buf.checksum.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Exact equality of two threshold sets. Floats are compared by bit pattern (as
/// Safe Mode does when computing dirtiness) so a confirmed apply is bit-for-bit,
/// not merely approximately, equal.
fn thresholds_eq(a: &Thresholds, b: &Thresholds) -> bool {
    a.repetition.to_bits() == b.repetition.to_bits()
        && a.self_referential.to_bits() == b.self_referential.to_bits()
        && a.token_velocity.to_bits() == b.token_velocity.to_bits()
        && a.tool_retry.to_bits() == b.tool_retry.to_bits()
        && a.soft.to_bits() == b.soft.to_bits()
        && a.medium.to_bits() == b.medium.to_bits()
        && a.hard.to_bits() == b.hard.to_bits()
        && a.window == b.window
}

// ===========================================================================
// Tests — exercise the pure decision core and helpers. Like the sibling
// Sentinel modules, these run under a host `std` harness if a lib target is
// ever introduced; they are excluded from the `no_std` kernel build (`cfg(test)`
// is off there). They do NOT touch the fixed address or the global singletons.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::safemode::sentinel_config::MigrationBuffer;

    fn cfg() -> Thresholds {
        Thresholds::default_config()
    }

    #[test]
    fn cold_boot_garbage_is_no_buffer() {
        // A buffer whose magic is wrong is treated as "nothing staged".
        let mut buf = MigrationBuffer::sealed_from_thresholds(&cfg(), true, 1);
        buf.magic = 0xdead_beef;
        assert!(matches!(decide(&buf), Decision::NoBuffer));
    }

    #[test]
    fn clean_authenticated_buffer_is_no_change() {
        let buf = MigrationBuffer::sealed_from_thresholds(&cfg(), false, 7);
        assert!(matches!(decide(&buf), Decision::NoChange));
    }

    #[test]
    fn dirty_authenticated_buffer_applies() {
        let mut want = cfg();
        want.repetition = 0.55;
        want.window = 32;
        let buf = MigrationBuffer::sealed_from_thresholds(&want, true, 9);
        match decide(&buf) {
            Decision::Apply(t) => {
                assert!(thresholds_eq(&t, &want));
            }
            other => panic!("expected Apply, got {:?}", core::mem::discriminant(&other)),
        }
    }

    #[test]
    fn tampered_checksum_is_rejected() {
        let mut buf = MigrationBuffer::sealed_from_thresholds(&cfg(), true, 3);
        // Forge a field without re-sealing → stored checksum no longer matches.
        buf.hard = 0.123;
        assert!(matches!(
            decide(&buf),
            Decision::Reject(MigrationError::ChecksumMismatch, _)
        ));
    }

    #[test]
    fn unknown_version_is_rejected() {
        // Bump the version then re-seal so the checksum is valid for the new
        // bytes — isolating the version check from the checksum check.
        let mut buf = MigrationBuffer::sealed_from_thresholds(&cfg(), true, 4);
        buf.version = MIGRATION_VERSION + 1;
        buf.seal();
        assert!(matches!(
            decide(&buf),
            Decision::Reject(MigrationError::UnsupportedVersion { .. }, _)
        ));
    }

    #[test]
    fn checksum_hex_is_64_lowercase_hex() {
        let buf = MigrationBuffer::sealed_from_thresholds(&cfg(), true, 1);
        let hex = checksum_hex(&buf);
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn thresholds_eq_detects_difference() {
        let a = cfg();
        let mut b = cfg();
        assert!(thresholds_eq(&a, &b));
        b.medium = 0.71;
        assert!(!thresholds_eq(&a, &b));
    }

    #[test]
    fn error_reasons_are_stable() {
        assert!(MigrationError::ChecksumMismatch.reason().contains("checksum"));
        assert!(MigrationError::ApplyFailed.reason().contains("did not take effect"));
        assert!(MigrationError::UnsupportedVersion { found: 2, expected: 1 }
            .reason()
            .contains("version"));
    }
}
