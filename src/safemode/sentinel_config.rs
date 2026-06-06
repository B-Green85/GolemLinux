//! Safe Mode — Sentinel configuration interface and the migration buffer.
//!
//! Safe Mode is the only context permitted to *write* Sentinel's tuning
//! knobs. It is an offline, single-operator, serial-terminal session: the main
//! OS is not running, the agent fleet is not registered, and the operator sits
//! at QEMU's COM1 console editing thresholds by hand. Nothing here touches the
//! live [`Monitor`](crate::sentinel::monitor::Monitor) directly — that would be
//! a write into a subsystem that is supposed to be quiescent. Instead the
//! operator's edits are sealed into a **migration buffer** at a fixed physical
//! address, and the *next* main-OS boot replays that buffer into the monitor
//! via `migration.rs` (Agent 3). This keeps Safe Mode strictly write-once and
//! makes every configuration change an auditable, integrity-checked artifact.
//!
//! ## Operator commands (typed over serial)
//!
//! | Command                | Effect                                              |
//! |------------------------|-----------------------------------------------------|
//! | `show`                 | Print the current working configuration.            |
//! | `set <key> <value>`    | Validate and stage one threshold change.            |
//! | `save`                 | Seal the working config into the migration buffer.  |
//! | `exit`                 | Leave Safe Mode (migration fires on next boot).     |
//! | `help`                 | List commands and valid keys.                        |
//!
//! `save` and (when changes are unsaved) `exit` are **irreversible** and so
//! require a typed `yes` confirmation on the following line.
//!
//! ## Why a buffer and not a syscall?
//!
//! The migration buffer is the contract surface between this module and
//! `migration.rs`. It is a `#[repr(C)]` struct at a documented physical
//! address (see [`MIGRATION_BUFFER_ADDR`]) carrying a magic header, every
//! configurable threshold, a dirty flag, a monotonic timestamp, and a SHA-256
//! checksum over its own contents. Agent 3 reads the address, checks the magic
//! and the checksum, and only then applies the values. A cold boot leaves
//! garbage at the address; the magic + checksum is what lets migration tell
//! "Safe Mode wrote a real config" apart from "this is uninitialized DRAM".
//!
//! `no_std`-clean: the only external surface used is `alloc` (already global in
//! the kernel crate), the embedded SHA-256 from
//! [`crate::sentinel::audit::sha256_hex`], and the `Thresholds` type from
//! [`crate::sentinel::monitor`]. No wall clock, no floating-point parsing in
//! the hot path beyond operator-entered values.

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sentinel::audit::sha256_hex;
use crate::sentinel::monitor::Thresholds;

// ===========================================================================
// Migration buffer — owned by this module, consumed by migration.rs (Agent 3).
// ===========================================================================

/// Fixed physical address of the migration buffer.
///
/// **Choice and justification.** The Safe Mode tooling is specified against
/// QEMU's `virt` guest memory map. On that map the start of guest DRAM is
/// `0x4000_0000` (1 GiB); the guest kernel image is loaded at `RAM_BASE +
/// 0x80000` and QEMU places the flattened device tree near the base of the
/// window. To stay clear of both the image and the DTB — and clear of the
/// kernel heap, whose backing frames the allocator draws from the firmware
/// memory map — we reserve a single 4 KiB page **64 MiB into the DRAM window**:
///
/// ```text
///   RAM_BASE (0x4000_0000) ── DTB, kernel image, heap-backing frames …
///   0x4400_0000  ┌───────────────────────────────┐  ← MIGRATION_BUFFER_ADDR
///                │  MigrationBuffer (one page)    │
///   0x4400_1000  └───────────────────────────────┘
/// ```
///
/// This requires the guest to have at least 128 MiB of RAM (the QEMU default
/// comfortably exceeds it) and requires the bootloader to mark the page
/// `0x4400_0000..0x4400_1000` **reserved** so neither firmware nor the frame
/// allocator ever hands it out. Because the page is reserved (not freed) it
/// survives the warm reset between the Safe Mode session and the next main-OS
/// boot, which is exactly the lifetime the migration hand-off needs. A cold
/// boot zeroes/garbages it — caught by the magic-header + checksum validation.
///
/// Agent 3 (`migration.rs`) reads from **this constant** — it must never
/// hard-code the literal. The address is page-aligned by construction.
///
/// (Integration note: Golem's assembled kernel currently targets x86_64 rather
/// than the `virt` aarch64/riscv machine. `0x4400_0000` = 1 GiB + 64 MiB is a
/// valid physical RAM address on the x86_64 QEMU machine as well, so the same
/// constant and the same "reserve one page in the firmware map" requirement
/// carry over unchanged; only the firmware map plumbing differs by platform.)
pub const MIGRATION_BUFFER_ADDR: usize = 0x4400_0000;

/// Bytes reserved for the migration buffer at [`MIGRATION_BUFFER_ADDR`]. One
/// 4 KiB page — far larger than [`MigrationBuffer`] needs, leaving room for the
/// format to grow without moving the address Agent 3 depends on.
pub const MIGRATION_BUFFER_SIZE: usize = 4096;

/// Magic header identifying a buffer written by Safe Mode. ASCII `"GLMMIGR1"`
/// packed big-endian, mirroring the project's [`BOOT_HANDOFF_MAGIC`] style
/// (`crate::memory`). migration.rs checks this first: a mismatch means Safe
/// Mode never wrote here and the buffer must be ignored.
///
/// [`BOOT_HANDOFF_MAGIC`]: crate::memory
pub const MIGRATION_MAGIC: u64 = 0x474C_4D4D_4947_5231; // "GLMMIGR1"

/// Format version of the [`MigrationBuffer`] layout. Bumped on any field /
/// ordering change so migration.rs can refuse a buffer it does not understand.
pub const MIGRATION_VERSION: u32 = 1;

/// The configuration hand-off record. Written once by Safe Mode at
/// [`MIGRATION_BUFFER_ADDR`]; read and validated by `migration.rs` on the next
/// main-OS boot.
///
/// `#[repr(C)]` with naturally-aligned fields in declared order gives a stable,
/// padding-free 88-byte layout that both writer and reader compile identically:
///
/// ```text
///   offset  size  field
///   ──────  ────  ─────────────────────────────────────────
///     0       8   magic            u64
///     8       4   version          u32
///    12       1   dirty            bool
///    13       3   _reserved        [u8; 3]   (explicit; keeps layout stable)
///    16       4   repetition       f32
///    20       4   self_referential f32
///    24       4   token_velocity   f32
///    28       4   tool_retry       f32
///    32       4   soft             f32
///    36       4   medium           f32
///    40       4   hard             f32
///    44       4   window           u32
///    48       8   timestamp        u64
///    56      32   checksum         [u8; 32]  (SHA-256 over all of the above)
///   ──────────────────────────────────────────────────────── total 88 bytes
/// ```
///
/// The checksum is computed over a canonical little-endian serialization of
/// every field *except itself* (see [`MigrationBuffer::compute_checksum`]); the
/// `_reserved` bytes are not part of that serialization, so platform-specific
/// padding can never affect the digest.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MigrationBuffer {
    /// Validation magic. Always [`MIGRATION_MAGIC`] when sealed by Safe Mode.
    pub magic: u64,
    /// Layout version. Always [`MIGRATION_VERSION`] when sealed.
    pub version: u32,
    /// True iff any threshold differs from the state Safe Mode loaded at the
    /// start of the session. migration.rs may skip applying a clean buffer.
    pub dirty: bool,
    /// Explicit padding so the layout is identical on every target and so the
    /// `f32` block below starts at a documented offset.
    pub _reserved: [u8; 3],

    // --- Sentinel signal thresholds ---
    /// Repetition-score gate (`RepetitionScore`). Default 0.6.
    pub repetition: f32,
    /// Self-referential-loop gate (`SelfReferentialLoop`). Default 0.5.
    pub self_referential: f32,
    /// Token-velocity-stall gate (`TokenVelocityStall`). Default 0.5.
    pub token_velocity: f32,
    /// Tool-retry-anomaly gate (`ToolRetryAnomaly`). Default 0.4.
    pub tool_retry: f32,

    // --- Control / response-tier thresholds ---
    /// Soft tier cumulative threshold. Default 0.4.
    pub soft: f32,
    /// Medium tier cumulative threshold. Default 0.7.
    pub medium: f32,
    /// Hard tier cumulative threshold. Default 0.9.
    pub hard: f32,
    /// Sliding observation window width. Default 16.
    pub window: u32,

    /// Monotonic counter value at seal time (see [`monotonic_now`]). Not a wall
    /// clock — strictly increasing, used to order writes.
    pub timestamp: u64,
    /// SHA-256 over the canonical serialization of all preceding logical
    /// fields. Zeroed until [`MigrationBuffer::seal`] runs.
    pub checksum: [u8; 32],
}

// The buffer must fit in its reserved page, and `bool` must really be one byte
// here or the documented offsets are wrong. Both are checked at compile time.
const _: () = assert!(core::mem::size_of::<MigrationBuffer>() <= MIGRATION_BUFFER_SIZE);
const _: () = assert!(core::mem::size_of::<MigrationBuffer>() == 88);

impl MigrationBuffer {
    /// Build an *unsealed* buffer from a [`Thresholds`] snapshot. The checksum
    /// is left zeroed; call [`seal`](Self::seal) (or use
    /// [`sealed_from_thresholds`](Self::sealed_from_thresholds)) before writing.
    pub fn from_thresholds(t: &Thresholds, dirty: bool, timestamp: u64) -> Self {
        Self {
            magic: MIGRATION_MAGIC,
            version: MIGRATION_VERSION,
            dirty,
            _reserved: [0; 3],
            repetition: t.repetition,
            self_referential: t.self_referential,
            token_velocity: t.token_velocity,
            tool_retry: t.tool_retry,
            soft: t.soft,
            medium: t.medium,
            hard: t.hard,
            window: t.window as u32,
            timestamp,
            checksum: [0; 32],
        }
    }

    /// Build a fully sealed buffer ready to write to the fixed address.
    pub fn sealed_from_thresholds(t: &Thresholds, dirty: bool, timestamp: u64) -> Self {
        let mut buf = Self::from_thresholds(t, dirty, timestamp);
        buf.seal();
        buf
    }

    /// Reconstruct a [`Thresholds`] from the stored values. The bridge Agent 3
    /// uses to feed the live monitor after validation.
    pub fn to_thresholds(&self) -> Thresholds {
        Thresholds {
            repetition: self.repetition,
            self_referential: self.self_referential,
            token_velocity: self.token_velocity,
            tool_retry: self.tool_retry,
            soft: self.soft,
            medium: self.medium,
            hard: self.hard,
            window: self.window as usize,
        }
    }

    /// Canonical little-endian byte serialization of every field except the
    /// checksum, in fixed order. Hashing *this* (rather than the raw struct
    /// bytes) keeps the digest independent of any compiler padding.
    fn checksum_payload(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(56);
        v.extend_from_slice(&self.magic.to_le_bytes());
        v.extend_from_slice(&self.version.to_le_bytes());
        v.push(self.dirty as u8);
        v.extend_from_slice(&self.repetition.to_bits().to_le_bytes());
        v.extend_from_slice(&self.self_referential.to_bits().to_le_bytes());
        v.extend_from_slice(&self.token_velocity.to_bits().to_le_bytes());
        v.extend_from_slice(&self.tool_retry.to_bits().to_le_bytes());
        v.extend_from_slice(&self.soft.to_bits().to_le_bytes());
        v.extend_from_slice(&self.medium.to_bits().to_le_bytes());
        v.extend_from_slice(&self.hard.to_bits().to_le_bytes());
        v.extend_from_slice(&self.window.to_le_bytes());
        v.extend_from_slice(&self.timestamp.to_le_bytes());
        v
    }

    /// Compute the SHA-256 checksum over [`checksum_payload`](Self::checksum_payload).
    /// Reuses the kernel's single embedded SHA-256 ([`sha256_hex`]) and decodes
    /// its hex output to raw bytes, so there is exactly one SHA implementation
    /// in the tree.
    pub fn compute_checksum(&self) -> [u8; 32] {
        let hex = sha256_hex(&self.checksum_payload());
        hex64_to_bytes(&hex)
    }

    /// Stamp the buffer with its checksum. Idempotent: the checksum field is
    /// not part of the payload, so re-sealing the same data yields the same
    /// digest.
    pub fn seal(&mut self) {
        self.checksum = self.compute_checksum();
    }

    /// True iff the magic and version match and the stored checksum recomputes.
    /// This is the single call migration.rs should gate on before trusting any
    /// value in the buffer.
    pub fn is_valid(&self) -> bool {
        self.magic == MIGRATION_MAGIC
            && self.version == MIGRATION_VERSION
            && self.checksum == self.compute_checksum()
    }

    /// Write this buffer to the fixed migration address with a volatile store.
    ///
    /// # Safety
    /// [`MIGRATION_BUFFER_ADDR`] must be mapped, writable, reserved for this
    /// purpose, and not aliased by any other live object. In Safe Mode the
    /// operator session is the only writer and the main OS is not running, so
    /// these hold. The buffer should be [`seal`](Self::seal)ed first or
    /// migration.rs will reject it.
    pub unsafe fn write_to_fixed_address(&self) {
        let dst = MIGRATION_BUFFER_ADDR as *mut MigrationBuffer;
        core::ptr::write_volatile(dst, *self);
    }
}

/// Read the migration buffer from the fixed address with a volatile load.
/// Convenience entry point for `migration.rs`; the caller must still check
/// [`MigrationBuffer::is_valid`] before using any field.
///
/// # Safety
/// [`MIGRATION_BUFFER_ADDR`] must be mapped and readable. The returned struct
/// may be garbage on a cold boot — that is what `is_valid` guards against.
pub unsafe fn read_migration_buffer() -> MigrationBuffer {
    core::ptr::read_volatile(MIGRATION_BUFFER_ADDR as *const MigrationBuffer)
}

/// Decode a 64-char lowercase hex string (the exact shape [`sha256_hex`]
/// returns) into 32 bytes. Non-hex bytes decode as 0; in practice the input is
/// always well-formed because it comes straight from `sha256_hex`.
fn hex64_to_bytes(hex: &str) -> [u8; 32] {
    let b = hex.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(*b.get(2 * i).unwrap_or(&b'0'));
        let lo = hex_nibble(*b.get(2 * i + 1).unwrap_or(&b'0'));
        out[i] = (hi << 4) | lo;
    }
    out
}

#[inline]
fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

// ===========================================================================
// Monotonic timestamp source.
// ===========================================================================

/// Strictly-increasing counter. Starts at 1 so a zeroed buffer (timestamp 0)
/// is trivially distinguishable from any Safe Mode write.
static MONOTONIC: AtomicU64 = AtomicU64::new(1);

/// Next monotonic timestamp. **Not** a wall clock — `no_std` has none. The
/// value only guarantees that a later seal has a larger stamp than an earlier
/// one, which is all migration.rs needs to order writes. Mirrors `audit.rs`'s
/// decision to keep timestamps an input rather than reading a clock, so the
/// buffer logic stays a pure function of its arguments.
pub fn monotonic_now() -> u64 {
    MONOTONIC.fetch_add(1, Ordering::Relaxed)
}

// ===========================================================================
// Validation.
// ===========================================================================

/// The keys `set` understands. Closed set on purpose: an unknown key is a typo,
/// not a feature request.
pub const VALID_KEYS: [&str; 8] = [
    "repetition",
    "self_referential",
    "token_velocity",
    "tool_retry",
    "soft",
    "medium",
    "hard",
    "window",
];

/// Upper bound on the observation window. The monitor keeps `window`
/// observations per agent in a `VecDeque`; an unbounded value here is a memory
/// foot-gun, so Safe Mode caps it.
pub const MAX_WINDOW: u32 = 4096;

/// Validate a single key/value edit against per-field rules. Returns the parsed
/// numeric value applied to a *copy* of `base`, or a human-readable error to
/// echo to the operator. Range rules only — cross-field rules (tier ordering)
/// are checked at save time by [`validate_config`].
fn apply_set(base: &Thresholds, key: &str, raw: &str) -> Result<Thresholds, String> {
    let mut t = *base;
    match key {
        "window" => {
            let w: u32 = raw
                .parse()
                .map_err(|_| format!("'{raw}' is not a whole number"))?;
            if w < 1 || w > MAX_WINDOW {
                return Err(format!("window must be in 1..={MAX_WINDOW}, got {w}"));
            }
            t.window = w as usize;
        }
        "repetition" | "self_referential" | "token_velocity" | "tool_retry" | "soft"
        | "medium" | "hard" => {
            let v: f32 = raw
                .parse()
                .map_err(|_| format!("'{raw}' is not a number"))?;
            if !v.is_finite() || v < 0.0 || v > 1.0 {
                return Err(format!("{key} must be in 0.0..=1.0, got {raw}"));
            }
            match key {
                "repetition" => t.repetition = v,
                "self_referential" => t.self_referential = v,
                "token_velocity" => t.token_velocity = v,
                "tool_retry" => t.tool_retry = v,
                "soft" => t.soft = v,
                "medium" => t.medium = v,
                "hard" => t.hard = v,
                _ => unreachable!(),
            }
        }
        _ => {
            return Err(format!(
                "unknown key '{key}'. valid keys: {}",
                VALID_KEYS.join(", ")
            ));
        }
    }
    Ok(t)
}

/// Whole-config invariants enforced before a save is allowed to commit. Range
/// checks already ran at `set` time; here we enforce the cross-field rule the
/// monitor's tier logic relies on: `soft <= medium <= hard`. Without it the
/// response tiers stop being monotonic and `tier_for` becomes nonsensical.
pub fn validate_config(t: &Thresholds) -> Result<(), String> {
    if !(t.soft <= t.medium && t.medium <= t.hard) {
        return Err(format!(
            "control thresholds must satisfy soft <= medium <= hard \
             (got soft={}, medium={}, hard={})",
            t.soft, t.medium, t.hard
        ));
    }
    if t.window < 1 || t.window as u64 > MAX_WINDOW as u64 {
        return Err(format!("window must be in 1..={MAX_WINDOW}, got {}", t.window));
    }
    for (name, v) in [
        ("repetition", t.repetition),
        ("self_referential", t.self_referential),
        ("token_velocity", t.token_velocity),
        ("tool_retry", t.tool_retry),
        ("soft", t.soft),
        ("medium", t.medium),
        ("hard", t.hard),
    ] {
        if !v.is_finite() || v < 0.0 || v > 1.0 {
            return Err(format!("{name} out of range 0.0..=1.0: {v}"));
        }
    }
    Ok(())
}

/// True iff two configs differ in any field (exact, bit-for-bit on the floats).
fn thresholds_differ(a: &Thresholds, b: &Thresholds) -> bool {
    a.repetition.to_bits() != b.repetition.to_bits()
        || a.self_referential.to_bits() != b.self_referential.to_bits()
        || a.token_velocity.to_bits() != b.token_velocity.to_bits()
        || a.tool_retry.to_bits() != b.tool_retry.to_bits()
        || a.soft.to_bits() != b.soft.to_bits()
        || a.medium.to_bits() != b.medium.to_bits()
        || a.hard.to_bits() != b.hard.to_bits()
        || a.window != b.window
}

// ===========================================================================
// Command session — the pure, testable core of the serial interface.
// ===========================================================================

/// What the operator's last line resolved to. The driver loop reads this to
/// decide whether to write the buffer and whether to leave Safe Mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// Stay in the session; just print [`Response::text`].
    Continue,
    /// A save committed this line — [`Response::buffer`] holds the sealed
    /// buffer the driver must write to the fixed address.
    Saved,
    /// The operator chose to leave Safe Mode. Migration fires on next boot.
    Exit,
}

/// The result of feeding one input line to a [`ConfigSession`].
#[derive(Debug, Clone)]
pub struct Response {
    /// Text to emit to the serial console.
    pub text: String,
    /// What the driver should do next.
    pub action: SessionAction,
    /// Present only when `action == Saved`: the sealed buffer to persist. Kept
    /// out of the session so the unsafe hardware write lives entirely in the
    /// driver and the session stays unit-testable.
    pub buffer: Option<MigrationBuffer>,
}

impl Response {
    fn cont(text: String) -> Self {
        Self { text, action: SessionAction::Continue, buffer: None }
    }
}

/// Pending two-step confirmation. `save` and unsaved-`exit` both arm one of
/// these; the next line must be `yes` to proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    SaveConfirm,
    ExitConfirm,
}

/// A Safe Mode configuration session. Holds the config the operator loaded
/// (`original`), the edited copy (`working`), and any pending confirmation.
/// Drive it line-by-line with [`handle_line`](Self::handle_line).
pub struct ConfigSession {
    original: Thresholds,
    working: Thresholds,
    pending: Pending,
    /// Whether a save has committed during this session.
    committed: bool,
}

impl ConfigSession {
    /// Start a session from the currently-effective thresholds. In production
    /// the caller passes the live `Monitor::thresholds()`; offline tools may
    /// pass [`Thresholds::default_config`].
    pub fn new(current: Thresholds) -> Self {
        Self {
            original: current,
            working: current,
            pending: Pending::None,
            committed: false,
        }
    }

    /// True iff the working config differs from what was loaded.
    pub fn is_dirty(&self) -> bool {
        thresholds_differ(&self.working, &self.original)
    }

    /// The current working configuration (for callers that want to inspect it
    /// without re-running `show`).
    pub fn working(&self) -> Thresholds {
        self.working
    }

    /// Feed one line of operator input. `timestamp` is sampled by the driver
    /// (via [`monotonic_now`]) and only consumed when a save actually commits.
    pub fn handle_line(&mut self, line: &str, timestamp: u64) -> Response {
        let line = line.trim();

        // A pending confirmation swallows the next line entirely.
        match self.pending {
            Pending::SaveConfirm => return self.resolve_save_confirm(line, timestamp),
            Pending::ExitConfirm => return self.resolve_exit_confirm(line),
            Pending::None => {}
        }

        if line.is_empty() {
            return Response::cont(String::new());
        }

        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        match cmd {
            "show" => Response::cont(self.render_show()),
            "help" => Response::cont(render_help()),
            "set" => {
                let key = parts.next().map(|k| k.to_ascii_lowercase());
                let value = parts.next();
                let extra = parts.next();
                self.cmd_set(key.as_deref(), value, extra.is_some())
            }
            "save" => self.cmd_save(),
            "exit" | "quit" => self.cmd_exit(),
            other => Response::cont(format!(
                "unknown command '{other}'. type 'help' for the command list.\n"
            )),
        }
    }

    fn cmd_set(&mut self, key: Option<&str>, value: Option<&str>, extra: bool) -> Response {
        let (key, value) = match (key, value) {
            (Some(k), Some(v)) => (k, v),
            _ => {
                return Response::cont(
                    "usage: set <key> <value>   (try 'help' for valid keys)\n".to_string(),
                )
            }
        };
        if extra {
            return Response::cont(
                "usage: set <key> <value>   (one value only)\n".to_string(),
            );
        }
        match apply_set(&self.working, key, value) {
            Ok(updated) => {
                self.working = updated;
                Response::cont(format!(
                    "ok: {key} = {value}   ({} unsaved change(s))\n",
                    self.changed_field_count()
                ))
            }
            Err(e) => Response::cont(format!("rejected: {e}\n")),
        }
    }

    fn cmd_save(&mut self) -> Response {
        if !self.is_dirty() {
            return Response::cont(
                "nothing to save: working config matches the loaded config.\n".to_string(),
            );
        }
        if let Err(e) = validate_config(&self.working) {
            return Response::cont(format!(
                "cannot save: {e}\nfix the value(s) with 'set' and try again.\n"
            ));
        }
        self.pending = Pending::SaveConfirm;
        Response::cont(format!(
            "ABOUT TO SAVE {} change(s) to the migration buffer at {:#x}.\n\
             This is IRREVERSIBLE and takes effect on the next main-OS boot.\n\
             Type 'yes' to confirm, anything else to cancel.\n",
            self.changed_field_count(),
            MIGRATION_BUFFER_ADDR
        ))
    }

    fn resolve_save_confirm(&mut self, line: &str, timestamp: u64) -> Response {
        self.pending = Pending::None;
        if line != "yes" {
            return Response::cont("save cancelled. no changes written.\n".to_string());
        }
        // Re-validate at the moment of commit — defensive, in case of any
        // unexpected state. dirty is always true here (guarded in cmd_save).
        if let Err(e) = validate_config(&self.working) {
            return Response::cont(format!("save aborted: {e}\n"));
        }
        let dirty = self.is_dirty();
        let buf = MigrationBuffer::sealed_from_thresholds(&self.working, dirty, timestamp);
        // The save is now the new baseline: further edits compare against it.
        self.original = self.working;
        self.committed = true;
        let mut text = String::new();
        text.push_str("saved. migration buffer sealed:\n");
        text.push_str(&format!("  address    : {MIGRATION_BUFFER_ADDR:#x}\n"));
        text.push_str(&format!("  magic      : {MIGRATION_MAGIC:#018x}\n"));
        text.push_str(&format!("  version    : {MIGRATION_VERSION}\n"));
        text.push_str(&format!("  dirty      : {dirty}\n"));
        text.push_str(&format!("  timestamp  : {timestamp}\n"));
        text.push_str(&format!("  checksum   : {}\n", bytes_to_hex(&buf.checksum)));
        text.push_str("migration will apply this config on the next main-OS boot.\n");
        Response { text, action: SessionAction::Saved, buffer: Some(buf) }
    }

    fn cmd_exit(&mut self) -> Response {
        if self.is_dirty() {
            self.pending = Pending::ExitConfirm;
            return Response::cont(
                "you have UNSAVED changes that will be lost on exit.\n\
                 type 'yes' to exit anyway, or anything else to stay and 'save' first.\n"
                    .to_string(),
            );
        }
        Response {
            text: exit_text(self.committed),
            action: SessionAction::Exit,
            buffer: None,
        }
    }

    fn resolve_exit_confirm(&mut self, line: &str) -> Response {
        self.pending = Pending::None;
        if line != "yes" {
            return Response::cont("staying in Safe Mode. unsaved changes kept.\n".to_string());
        }
        Response {
            text: exit_text(self.committed),
            action: SessionAction::Exit,
            buffer: None,
        }
    }

    fn changed_field_count(&self) -> usize {
        let o = &self.original;
        let w = &self.working;
        let mut n = 0;
        if w.repetition.to_bits() != o.repetition.to_bits() { n += 1; }
        if w.self_referential.to_bits() != o.self_referential.to_bits() { n += 1; }
        if w.token_velocity.to_bits() != o.token_velocity.to_bits() { n += 1; }
        if w.tool_retry.to_bits() != o.tool_retry.to_bits() { n += 1; }
        if w.soft.to_bits() != o.soft.to_bits() { n += 1; }
        if w.medium.to_bits() != o.medium.to_bits() { n += 1; }
        if w.hard.to_bits() != o.hard.to_bits() { n += 1; }
        if w.window != o.window { n += 1; }
        n
    }

    fn render_show(&self) -> String {
        let w = &self.working;
        let mark = |changed: bool| if changed { " *" } else { "" };
        let o = &self.original;
        let mut s = String::new();
        s.push_str("current Safe Mode configuration ('*' = unsaved change):\n");
        s.push_str("  signal thresholds:\n");
        s.push_str(&format!(
            "    repetition         = {}{}\n",
            w.repetition,
            mark(w.repetition.to_bits() != o.repetition.to_bits())
        ));
        s.push_str(&format!(
            "    self_referential   = {}{}\n",
            w.self_referential,
            mark(w.self_referential.to_bits() != o.self_referential.to_bits())
        ));
        s.push_str(&format!(
            "    token_velocity     = {}{}\n",
            w.token_velocity,
            mark(w.token_velocity.to_bits() != o.token_velocity.to_bits())
        ));
        s.push_str(&format!(
            "    tool_retry         = {}{}\n",
            w.tool_retry,
            mark(w.tool_retry.to_bits() != o.tool_retry.to_bits())
        ));
        s.push_str("  control thresholds:\n");
        s.push_str(&format!(
            "    soft               = {}{}\n",
            w.soft,
            mark(w.soft.to_bits() != o.soft.to_bits())
        ));
        s.push_str(&format!(
            "    medium             = {}{}\n",
            w.medium,
            mark(w.medium.to_bits() != o.medium.to_bits())
        ));
        s.push_str(&format!(
            "    hard               = {}{}\n",
            w.hard,
            mark(w.hard.to_bits() != o.hard.to_bits())
        ));
        s.push_str("  window:\n");
        s.push_str(&format!(
            "    window             = {}{}\n",
            w.window,
            mark(w.window != o.window)
        ));
        s.push_str(&format!(
            "  status: {}\n",
            if self.is_dirty() { "MODIFIED (unsaved)" } else { "clean" }
        ));
        s
    }
}

fn exit_text(committed: bool) -> String {
    if committed {
        "exiting Safe Mode. the saved config will be migrated on the next \
         main-OS boot.\n"
            .to_string()
    } else {
        "exiting Safe Mode. no config was saved; the main OS will boot with \
         its existing thresholds.\n"
            .to_string()
    }
}

fn render_help() -> String {
    let mut s = String::new();
    s.push_str("Safe Mode — Sentinel configuration\n");
    s.push_str("commands:\n");
    s.push_str("  show                 show current working config\n");
    s.push_str("  set <key> <value>    change one value (validated)\n");
    s.push_str("  save                 seal config to the migration buffer (confirm)\n");
    s.push_str("  exit                 leave Safe Mode (migration on next boot)\n");
    s.push_str("  help                 this message\n");
    s.push_str(&format!("keys: {}\n", VALID_KEYS.join(", ")));
    s.push_str("  signal/control thresholds: 0.0..=1.0   window: 1..=4096\n");
    s
}

fn bytes_to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

// ===========================================================================
// Serial driver — the thin, hardware-facing shell around ConfigSession.
// ===========================================================================

/// How a [`run_safe_mode_console`] session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeModeOutcome {
    /// Operator exited after at least one committed save.
    ExitedWithSave,
    /// Operator exited without saving (no migration will occur).
    ExitedWithoutSave,
}

/// Run the interactive Safe Mode console on COM1 until the operator exits.
///
/// `current` is the configuration to load as the session baseline (the live
/// `Monitor::thresholds()` in production). On a confirmed `save` this writes the
/// sealed [`MigrationBuffer`] to [`MIGRATION_BUFFER_ADDR`]; `migration.rs` picks
/// it up on the next boot.
///
/// This is the only part of the module that touches hardware. It is gated to
/// `x86_64` because the COM1 port I/O is x86-specific; the [`ConfigSession`]
/// core above is portable and is what the unit tests exercise.
#[cfg(target_arch = "x86_64")]
pub fn run_safe_mode_console(current: Thresholds) -> SafeModeOutcome {
    let mut session = ConfigSession::new(current);

    // SAFETY: COM1 (0x3F8) is a fixed legacy port; Safe Mode runs at CPL 0.
    unsafe {
        serial::write_str(
            "\n=== Golem Linux Safe Mode — Sentinel configuration ===\n",
        );
        serial::write_str(&render_help());
        serial::write_str("\n> ");
    }

    let mut line = String::new();
    loop {
        // SAFETY: CPL 0, fixed COM1 port; read_line only issues port I/O.
        let ch = unsafe { serial::read_byte() };
        match ch {
            b'\r' | b'\n' => {
                let ts = monotonic_now();
                let resp = session.handle_line(&line, ts);
                line.clear();
                // SAFETY: see above — port-write-only.
                unsafe {
                    serial::write_str("\n");
                    serial::write_str(&resp.text);
                }
                if let Some(buf) = resp.buffer {
                    // SAFETY: the reserved migration page is mapped and
                    // writable, the main OS is not running, and we are the sole
                    // writer in Safe Mode. The buffer is sealed.
                    unsafe { buf.write_to_fixed_address() };
                }
                if resp.action == SessionAction::Exit {
                    break;
                }
                // SAFETY: see above.
                unsafe { serial::write_str("\n> ") };
            }
            0x08 | 0x7f => {
                // Backspace / DEL: drop the last char and visually erase it.
                if line.pop().is_some() {
                    // SAFETY: see above.
                    unsafe { serial::write_str("\x08 \x08") };
                }
            }
            byte => {
                if let Some(c) = char::from_u32(byte as u32) {
                    if !c.is_control() {
                        line.push(c);
                        // SAFETY: see above — echo the typed byte.
                        unsafe { serial::write_byte(byte) };
                    }
                }
            }
        }
    }

    if session.committed {
        SafeModeOutcome::ExitedWithSave
    } else {
        SafeModeOutcome::ExitedWithoutSave
    }
}

/// Minimal COM1 (0x3F8) polling driver. Mirrors the kernel's `serial_write`
/// primitive in `src/main.rs`: raw `out`/`in` with a line-status poll, adequate
/// for the low-volume, single-operator Safe Mode console under emulation.
#[cfg(target_arch = "x86_64")]
mod serial {
    /// COM1 data register.
    const DATA: u16 = 0x3F8;
    /// COM1 Line Status Register.
    const LSR: u16 = 0x3FD;
    /// LSR bit 0: Data Ready.
    const LSR_DATA_READY: u8 = 1 << 0;
    /// LSR bit 5: Transmit Holding Register Empty.
    const LSR_THR_EMPTY: u8 = 1 << 5;

    /// Write one byte to COM1, waiting for the transmit holding register.
    ///
    /// # Safety
    /// Caller must be at CPL 0 (privileged `in`/`out`). Touches no memory.
    pub unsafe fn write_byte(byte: u8) {
        loop {
            let status: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") LSR,
                out("al") status,
                options(nomem, nostack, preserves_flags),
            );
            if status & LSR_THR_EMPTY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        core::arch::asm!(
            "out dx, al",
            in("dx") DATA,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }

    /// Write a string to COM1 byte-by-byte.
    ///
    /// # Safety
    /// See [`write_byte`].
    pub unsafe fn write_str(s: &str) {
        for b in s.bytes() {
            write_byte(b);
        }
    }

    /// Block until a byte is available on COM1, then return it.
    ///
    /// # Safety
    /// Caller must be at CPL 0. Touches no memory.
    pub unsafe fn read_byte() -> u8 {
        loop {
            let status: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") LSR,
                out("al") status,
                options(nomem, nostack, preserves_flags),
            );
            if status & LSR_DATA_READY != 0 {
                let byte: u8;
                core::arch::asm!(
                    "in al, dx",
                    in("dx") DATA,
                    out("al") byte,
                    options(nomem, nostack, preserves_flags),
                );
                return byte;
            }
            core::hint::spin_loop();
        }
    }
}

// ===========================================================================
// Tests. Exercise the portable session + buffer logic; the COM1 driver is not
// invoked (it would block on hardware I/O and issue privileged instructions).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cfg() -> Thresholds {
        Thresholds::default_config()
    }

    #[test]
    fn buffer_layout_is_88_bytes() {
        assert_eq!(core::mem::size_of::<MigrationBuffer>(), 88);
        assert!(core::mem::size_of::<MigrationBuffer>() <= MIGRATION_BUFFER_SIZE);
    }

    #[test]
    fn magic_decodes_to_ascii() {
        assert_eq!(&MIGRATION_MAGIC.to_be_bytes(), b"GLMMIGR1");
    }

    #[test]
    fn sealed_buffer_validates() {
        let buf = MigrationBuffer::sealed_from_thresholds(&default_cfg(), true, 42);
        assert!(buf.is_valid());
        assert_eq!(buf.magic, MIGRATION_MAGIC);
        assert_eq!(buf.version, MIGRATION_VERSION);
        assert_eq!(buf.timestamp, 42);
        assert!(buf.dirty);
    }

    #[test]
    fn tampering_breaks_checksum() {
        let mut buf = MigrationBuffer::sealed_from_thresholds(&default_cfg(), true, 1);
        assert!(buf.is_valid());
        buf.hard = 0.123; // forge a value without re-sealing
        assert!(!buf.is_valid());
    }

    #[test]
    fn wrong_magic_is_invalid() {
        let mut buf = MigrationBuffer::sealed_from_thresholds(&default_cfg(), false, 1);
        buf.magic = 0;
        assert!(!buf.is_valid());
    }

    #[test]
    fn roundtrip_thresholds() {
        let mut cfg = default_cfg();
        cfg.repetition = 0.55;
        cfg.window = 32;
        let buf = MigrationBuffer::sealed_from_thresholds(&cfg, true, 7);
        let back = buf.to_thresholds();
        assert_eq!(back.repetition.to_bits(), 0.55f32.to_bits());
        assert_eq!(back.window, 32);
    }

    #[test]
    fn checksum_excludes_itself_and_is_stable() {
        let buf = MigrationBuffer::sealed_from_thresholds(&default_cfg(), true, 99);
        let recomputed = buf.compute_checksum();
        assert_eq!(buf.checksum, recomputed);
    }

    #[test]
    fn hex_roundtrip() {
        let hex = sha256_hex(b"golem");
        let bytes = hex64_to_bytes(&hex);
        assert_eq!(bytes_to_hex(&bytes), hex);
    }

    #[test]
    fn set_rejects_out_of_range() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("set repetition 1.5", 1);
        assert!(r.text.contains("rejected"));
        assert!(!s.is_dirty());
    }

    #[test]
    fn set_rejects_unknown_key() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("set bogus 0.5", 1);
        assert!(r.text.contains("unknown key"));
    }

    #[test]
    fn set_rejects_non_numeric() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("set soft high", 1);
        assert!(r.text.contains("rejected"));
    }

    #[test]
    fn set_accepts_valid_and_marks_dirty() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("set repetition 0.8", 1);
        assert!(r.text.contains("ok"));
        assert!(s.is_dirty());
        assert_eq!(s.working().repetition.to_bits(), 0.8f32.to_bits());
    }

    #[test]
    fn window_bounds_enforced() {
        let mut s = ConfigSession::new(default_cfg());
        assert!(s.handle_line("set window 0", 1).text.contains("rejected"));
        assert!(s.handle_line("set window 99999", 1).text.contains("rejected"));
        assert!(s.handle_line("set window 64", 1).text.contains("ok"));
        assert_eq!(s.working().window, 64);
    }

    #[test]
    fn save_requires_confirmation_and_writes_buffer() {
        let mut s = ConfigSession::new(default_cfg());
        s.handle_line("set medium 0.75", 1);
        let prompt = s.handle_line("save", 2);
        assert_eq!(prompt.action, SessionAction::Continue);
        assert!(prompt.text.contains("IRREVERSIBLE"));
        assert!(prompt.buffer.is_none());

        let done = s.handle_line("yes", 3);
        assert_eq!(done.action, SessionAction::Saved);
        let buf = done.buffer.expect("save must produce a buffer");
        assert!(buf.is_valid());
        assert_eq!(buf.medium.to_bits(), 0.75f32.to_bits());
        assert_eq!(buf.timestamp, 3);
        assert!(buf.dirty);
        // After a save the working set becomes the baseline → clean again.
        assert!(!s.is_dirty());
    }

    #[test]
    fn save_can_be_cancelled() {
        let mut s = ConfigSession::new(default_cfg());
        s.handle_line("set hard 0.95", 1);
        s.handle_line("save", 2);
        let cancel = s.handle_line("no", 3);
        assert_eq!(cancel.action, SessionAction::Continue);
        assert!(cancel.text.contains("cancelled"));
        assert!(cancel.buffer.is_none());
        // Change is still pending — not lost, just not written.
        assert!(s.is_dirty());
    }

    #[test]
    fn save_clean_config_is_noop() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("save", 1);
        assert!(r.text.contains("nothing to save"));
        assert_eq!(r.action, SessionAction::Continue);
    }

    #[test]
    fn save_blocked_when_tiers_out_of_order() {
        let mut s = ConfigSession::new(default_cfg());
        // medium above hard violates soft <= medium <= hard.
        s.handle_line("set medium 0.95", 1); // hard default 0.9
        let r = s.handle_line("save", 2);
        assert!(r.text.contains("cannot save"));
        assert!(r.text.contains("soft <= medium <= hard"));
    }

    #[test]
    fn exit_clean_leaves_immediately() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("exit", 1);
        assert_eq!(r.action, SessionAction::Exit);
    }

    #[test]
    fn exit_with_unsaved_changes_requires_confirm() {
        let mut s = ConfigSession::new(default_cfg());
        s.handle_line("set soft 0.3", 1);
        let warn = s.handle_line("exit", 2);
        assert_eq!(warn.action, SessionAction::Continue);
        assert!(warn.text.contains("UNSAVED"));

        let stay = s.handle_line("no", 3);
        assert_eq!(stay.action, SessionAction::Continue);
        assert!(s.is_dirty());

        s.handle_line("exit", 4);
        let leave = s.handle_line("yes", 5);
        assert_eq!(leave.action, SessionAction::Exit);
    }

    #[test]
    fn show_reflects_pending_edits() {
        let mut s = ConfigSession::new(default_cfg());
        s.handle_line("set tool_retry 0.42", 1);
        let r = s.handle_line("show", 2);
        assert!(r.text.contains("tool_retry"));
        assert!(r.text.contains("MODIFIED"));
        assert!(r.text.contains('*')); // change marker
    }

    #[test]
    fn unknown_command_is_friendly() {
        let mut s = ConfigSession::new(default_cfg());
        let r = s.handle_line("frobnicate", 1);
        assert!(r.text.contains("unknown command"));
    }

    #[test]
    fn validate_config_accepts_defaults() {
        assert!(validate_config(&default_cfg()).is_ok());
    }
}
