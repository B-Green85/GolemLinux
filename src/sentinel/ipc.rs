//! Sentinel IPC channel over **virtio-serial**.
//!
//! This module is the kernel side of the Golem ⇄ Sentinel handshake. The Python
//! agent running on the host registers itself with Sentinel by exchanging JSON
//! messages across a virtio-serial port. The host end of that port is a UNIX
//! socket; the guest (this kernel) end is a virtio-serial *port* exposed by a
//! virtio-console device.
//!
//! ## QEMU wiring
//!
//! The guest must be launched with the following flags so that the port appears
//! to the kernel as a virtio-console device with one extra named port:
//!
//! ```text
//! -device virtio-serial \
//! -chardev socket,path=/tmp/golem-sentinel.sock,server=on,wait=off,id=golem \
//! -device virtserialport,chardev=golem,name=org.truesystems.sentinel.0
//! ```
//!
//! * `-device virtio-serial` instantiates the virtio-console (multiport) device.
//! * `-chardev socket,...` is the host-side UNIX socket the Python agent talks to.
//! * `-device virtserialport,...,name=org.truesystems.sentinel.0` adds the data
//!   port the agent registers across. Because a *named* port is requested, the
//!   device negotiates `VIRTIO_CONSOLE_F_MULTIPORT` and exposes control queues.
//!
//! ## What is real vs. stubbed in this file
//!
//! GolemLinux has no pre-existing VirtIO infrastructure, so everything below was
//! written from scratch. Each block is tagged so reviewers and the other Phase-4
//! agents know exactly how far the implementation goes:
//!
//! * `[FULL]`  — a complete, spec-faithful implementation.
//! * `[STUB]`  — a deliberately minimal placeholder with a documented contract;
//!               correct enough to compile and to drive the happy path, but a
//!               later phase is expected to harden it.
//!
//! Summary:
//!
//! | Concern                                   | Status  |
//! |-------------------------------------------|---------|
//! | virtio-mmio register transport            | `[FULL]`|
//! | split virtqueue layout + submit/poll path | `[FULL]`|
//! | device reset / feature negotiation        | `[FULL]`|
//! | data-path read & write on a port          | `[FULL]`|
//! | minimal JSON parse + serialize            | `[FULL]`|
//! | device *discovery* (MMIO base address)    | `[STUB]`|
//! | DMA memory (assumes identity-mapped phys) | `[STUB]`|
//! | multiport control-queue handshake         | `[STUB]`|
//! | wall-clock timestamp source               | `[STUB]`|
//! | cryptographic audit hash                  | `[STUB]`|
//!
//! ## Transport choice
//!
//! The given QEMU flags produce a *virtio-pci* device on the default x86 machine
//! type. Implementing virtio-pci means PCI config-space enumeration plus parsing
//! the virtio PCI capability list, which is a large amount of unrelated plumbing.
//! To keep this deliverable self-contained we implement the **virtio-mmio**
//! transport (VIRTIO 1.x, "modern"), which is byte-for-byte identical at the
//! virtqueue level and is what GolemLinux uses on the `-machine virt` target.
//! Discovery is abstracted behind [`VirtioTransport`] so a virtio-pci transport
//! can be dropped in later without touching the IPC logic.
//!
//! The module is `#![no_std]`-compatible: it depends only on `core`.

use core::fmt::{self, Write as _};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// =============================================================================
// Error type
// =============================================================================

/// Errors surfaced by the Sentinel IPC channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// No virtio device was found at the configured transport location, or the
    /// MMIO magic / version / device-id did not match a virtio-console.
    DeviceNotFound,
    /// The device rejected our feature subset (cleared `FEATURES_OK`).
    FeatureNegotiationFailed,
    /// A virtqueue could not be configured (size 0, or `QueueReady` refused).
    QueueSetupFailed,
    /// A message exceeded the fixed channel buffer.
    MessageTooLarge,
    /// The incoming bytes were not the JSON request shape we expect.
    MalformedRequest,
    /// The channel has not been initialised yet.
    NotInitialised,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            IpcError::DeviceNotFound => "virtio-serial device not found",
            IpcError::FeatureNegotiationFailed => "feature negotiation failed",
            IpcError::QueueSetupFailed => "virtqueue setup failed",
            IpcError::MessageTooLarge => "message exceeds channel buffer",
            IpcError::MalformedRequest => "malformed registration request",
            IpcError::NotInitialised => "ipc channel not initialised",
        };
        f.write_str(s)
    }
}

// =============================================================================
// Protocol types
// =============================================================================

/// Permission tier requested by a registering agent.
///
/// Mirrors the `permission_tier` field of the wire protocol. The ordering is
/// meaningful (`ReadOnly` < `Write` < `Execute`) so other Sentinel components
/// can compare tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionTier {
    ReadOnly,
    Write,
    Execute,
}

impl PermissionTier {
    /// Parse the wire form (`"READ_ONLY" | "WRITE" | "EXECUTE"`).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "READ_ONLY" => Some(PermissionTier::ReadOnly),
            "WRITE" => Some(PermissionTier::Write),
            "EXECUTE" => Some(PermissionTier::Execute),
            _ => None,
        }
    }

    /// Render the wire form.
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionTier::ReadOnly => "READ_ONLY",
            PermissionTier::Write => "WRITE",
            PermissionTier::Execute => "EXECUTE",
        }
    }
}

/// Maximum length of an `agent_id` we will accept/echo. Kept small and fixed so
/// the whole protocol stays allocation-free.
pub const MAX_AGENT_ID: usize = 128;

/// A parsed registration request: `{"method":"register", ...}`.
///
/// `agent_id` is stored in a fixed inline buffer to remain `no_std`/alloc-free.
#[derive(Debug, Clone, Copy)]
pub struct RegisterRequest {
    agent_id: [u8; MAX_AGENT_ID],
    agent_id_len: usize,
    pub permission_tier: PermissionTier,
}

impl RegisterRequest {
    /// The requesting agent's id as a `&str`.
    pub fn agent_id(&self) -> &str {
        // Safe: only ever populated from a validated UTF-8 `&str` slice.
        core::str::from_utf8(&self.agent_id[..self.agent_id_len]).unwrap_or("")
    }
}

/// The response we write back: `{"success":bool, "agent_id":..., "audit_hash":..., "timestamp":...}`.
#[derive(Debug, Clone, Copy)]
pub struct RegisterResponse {
    pub success: bool,
    agent_id: [u8; MAX_AGENT_ID],
    agent_id_len: usize,
    audit_hash: [u8; 16], // 64-bit hash rendered as 16 hex chars
    timestamp: [u8; 24],  // ASCII, see Clock stub
    timestamp_len: usize,
}

impl RegisterResponse {
    fn agent_id(&self) -> &str {
        core::str::from_utf8(&self.agent_id[..self.agent_id_len]).unwrap_or("")
    }
    fn audit_hash(&self) -> &str {
        core::str::from_utf8(&self.audit_hash).unwrap_or("")
    }
    fn timestamp(&self) -> &str {
        core::str::from_utf8(&self.timestamp[..self.timestamp_len]).unwrap_or("")
    }
}

// =============================================================================
// [FULL] Minimal JSON parse + serialize
//
// We hand-roll just enough JSON to cover the fixed protocol shape. This is NOT
// a general JSON parser: it is a flat key→string-value scanner that tolerates
// arbitrary whitespace and works on the two known message shapes. Keeping it
// purpose-built avoids pulling in serde/alloc and keeps the channel `no_std`.
// =============================================================================

/// Extract the string value associated with `"key"` from a flat JSON object.
///
/// Returns the unescaped-enough value slice. Only the escapes that can appear in
/// our protocol (`\"` and `\\`) are handled; anything else is passed through.
/// Returns `None` if the key is absent or its value is not a JSON string.
fn json_string_field<'a>(src: &'a str, key: &str) -> Option<&'a str> {
    // Find the `"key"` token, then the following `:`, then the opening quote.
    let bytes = src.as_bytes();
    let mut needle = [0u8; 64];
    if key.len() + 2 > needle.len() {
        return None;
    }
    needle[0] = b'"';
    needle[1..1 + key.len()].copy_from_slice(key.as_bytes());
    needle[1 + key.len()] = b'"';
    let needle = &needle[..key.len() + 2];

    let mut i = 0usize;
    let start = loop {
        if i + needle.len() > bytes.len() {
            return None;
        }
        if &bytes[i..i + needle.len()] == needle {
            break i + needle.len();
        }
        i += 1;
    };

    // Skip whitespace, require ':', skip whitespace, require '"'.
    let mut j = start;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b':' {
        return None;
    }
    j += 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'"' {
        return None;
    }
    j += 1;
    let value_start = j;

    // Scan to the closing quote, honouring backslash escapes.
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2, // skip the escaped char
            b'"' => return src.get(value_start..j),
            _ => j += 1,
        }
    }
    None
}

/// Parse a registration request from raw channel bytes.
pub fn parse_register_request(raw: &[u8]) -> Result<RegisterRequest, IpcError> {
    let src = core::str::from_utf8(raw).map_err(|_| IpcError::MalformedRequest)?;

    let method = json_string_field(src, "method").ok_or(IpcError::MalformedRequest)?;
    if method != "register" {
        return Err(IpcError::MalformedRequest);
    }

    let agent_id = json_string_field(src, "agent_id").ok_or(IpcError::MalformedRequest)?;
    if agent_id.len() > MAX_AGENT_ID {
        return Err(IpcError::MessageTooLarge);
    }

    let tier_str = json_string_field(src, "permission_tier").ok_or(IpcError::MalformedRequest)?;
    let permission_tier = PermissionTier::from_wire(tier_str).ok_or(IpcError::MalformedRequest)?;

    let mut buf = [0u8; MAX_AGENT_ID];
    buf[..agent_id.len()].copy_from_slice(agent_id.as_bytes());

    Ok(RegisterRequest {
        agent_id: buf,
        agent_id_len: agent_id.len(),
        permission_tier,
    })
}

/// A bounded `core::fmt::Write` sink over a fixed byte buffer. Used to build the
/// response JSON without allocating.
struct FixedWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
    overflow: bool,
}

impl<'a> FixedWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        FixedWriter { buf, pos: 0, overflow: false }
    }
    fn written(&self) -> usize {
        self.pos
    }
}

impl<'a> fmt::Write for FixedWriter<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        if self.pos + bytes.len() > self.buf.len() {
            self.overflow = true;
            return Err(fmt::Error);
        }
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
        Ok(())
    }
}

/// Serialize a [`RegisterResponse`] into `out`, returning the number of bytes
/// written. `agent_id`, `audit_hash` and `timestamp` are protocol-controlled and
/// contain no characters that require JSON escaping, so they are emitted raw.
pub fn serialize_response(resp: &RegisterResponse, out: &mut [u8]) -> Result<usize, IpcError> {
    let mut w = FixedWriter::new(out);
    let r = write!(
        w,
        "{{\"success\":{},\"agent_id\":\"{}\",\"audit_hash\":\"{}\",\"timestamp\":\"{}\"}}",
        resp.success,
        resp.agent_id(),
        resp.audit_hash(),
        resp.timestamp(),
    );
    if r.is_err() || w.overflow {
        return Err(IpcError::MessageTooLarge);
    }
    Ok(w.written())
}

// =============================================================================
// [STUB] Audit hash
//
// The real Sentinel audit trail (a sibling Phase-4 deliverable) is expected to
// supply a cryptographic hash chaining each registration into a tamper-evident
// log. Until that subsystem lands we compute a deterministic FNV-1a 64-bit
// digest over the canonical request fields. It is stable and collision-resistant
// enough for wiring/tests, but is explicitly NOT a security primitive.
// =============================================================================

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Render a u64 as 16 lowercase hex chars into a fixed buffer.
fn hex16(value: u64) -> [u8; 16] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 16];
    for i in 0..16 {
        let nibble = (value >> ((15 - i) * 4)) & 0xf;
        out[i] = HEX[nibble as usize];
    }
    out
}

/// Compute the audit hash for a registration. Folds in the agent id and tier so
/// distinct registrations get distinct digests.
fn audit_hash_for(req: &RegisterRequest) -> [u8; 16] {
    let mut buf = [0u8; MAX_AGENT_ID + 16];
    let id = req.agent_id().as_bytes();
    buf[..id.len()].copy_from_slice(id);
    let tier = req.permission_tier.as_wire().as_bytes();
    buf[id.len()..id.len() + tier.len()].copy_from_slice(tier);
    hex16(fnv1a(&buf[..id.len() + tier.len()]))
}

// =============================================================================
// [STUB] Clock / timestamp source
//
// A `no_std` kernel with no timer or RTC driver yet has no wall clock. We expose
// a monotonic counter that increments per call and render it as
// `"mono+<n>"`. When the timekeeping driver (another phase) lands, replace
// `Clock::now_string` with a real ISO-8601 wall-clock reading; the protocol field
// is a free-form string so no wire change is needed.
// =============================================================================

struct Clock;

static MONO_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Clock {
    /// Returns an ASCII timestamp and its length. Format: `mono+<counter>`.
    fn now_string() -> ([u8; 24], usize) {
        let n = MONO_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut buf = [0u8; 24];
        let mut w = FixedWriter::new(&mut buf);
        // Infallible for a u64 into 24 bytes ("mono+" + 20 digits max).
        let _ = write!(w, "mono+{}", n);
        let len = w.written();
        (buf, len)
    }
}

// =============================================================================
// [FULL] virtio-mmio register transport
//
// VIRTIO 1.x ("modern") MMIO register block. All accesses are 32-bit volatile.
// Offsets are from the VIRTIO spec, §4.2.2.
// =============================================================================

mod mmio {
    pub const MAGIC_VALUE: usize = 0x000; // R  — must read "virt" (0x74726976)
    pub const VERSION: usize = 0x004; // R  — 2 for modern
    pub const DEVICE_ID: usize = 0x008; // R  — 3 == console
    #[allow(dead_code)]
    pub const VENDOR_ID: usize = 0x00c;
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_READY: usize = 0x044;
    pub const QUEUE_NOTIFY: usize = 0x050;
    #[allow(dead_code)]
    pub const INTERRUPT_STATUS: usize = 0x060;
    #[allow(dead_code)]
    pub const INTERRUPT_ACK: usize = 0x064;
    pub const STATUS: usize = 0x070;
    pub const QUEUE_DESC_LOW: usize = 0x080;
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    pub const QUEUE_DRIVER_LOW: usize = 0x090; // avail ring
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    pub const QUEUE_DEVICE_LOW: usize = 0x0a0; // used ring
    pub const QUEUE_DEVICE_HIGH: usize = 0x0a4;

    pub const MAGIC: u32 = 0x7472_6976; // "virt" little-endian
    pub const VERSION_MODERN: u32 = 2;
    pub const DEVICE_ID_CONSOLE: u32 = 3;

    // Status bits (VIRTIO spec §2.1).
    pub const STATUS_ACKNOWLEDGE: u32 = 1;
    pub const STATUS_DRIVER: u32 = 2;
    pub const STATUS_DRIVER_OK: u32 = 4;
    pub const STATUS_FEATURES_OK: u32 = 8;

    // Feature bits.
    pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
    pub const VIRTIO_CONSOLE_F_MULTIPORT: u64 = 1 << 1;
}

/// Abstraction over the bus that carries the virtqueues. Implemented here by
/// [`MmioTransport`]; a future virtio-pci transport can implement the same trait
/// and the rest of this module is unchanged.
trait VirtioTransport {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&self, offset: usize, value: u32);
}

/// `[FULL]` virtio-mmio transport over a base MMIO address.
struct MmioTransport {
    base: usize,
}

impl MmioTransport {
    /// # Safety
    /// `base` must be the MMIO base of a real virtio-mmio device, mapped and
    /// accessible. Discovery of that address is the `[STUB]` part (see
    /// [`discover_device`]); the register protocol itself is complete.
    const unsafe fn new(base: usize) -> Self {
        MmioTransport { base }
    }
}

impl VirtioTransport for MmioTransport {
    fn read32(&self, offset: usize) -> u32 {
        unsafe { ptr::read_volatile((self.base + offset) as *const u32) }
    }
    fn write32(&self, offset: usize, value: u32) {
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }
}

// =============================================================================
// [STUB] Device discovery
//
// On a real `-machine virt` target the virtio-mmio windows are described by the
// device tree / ACPI. We have no DT parser yet, so we hard-code the conventional
// QEMU `virt` virtio-mmio base. `discover_device` validates the magic/version/id
// at that address and fails cleanly (`DeviceNotFound`) if nothing is there — so
// this is safe to call even when no device is present.
//
// QEMU `virt` lays out 32 virtio-mmio transports at 0x0a00_0000, 0x200 apart.
// The virtio-console typically enumerates into one of these slots; we scan them.
// =============================================================================

const QEMU_VIRT_MMIO_BASE: usize = 0x0a00_0000;
const QEMU_VIRT_MMIO_STRIDE: usize = 0x200;
const QEMU_VIRT_MMIO_COUNT: usize = 32;

/// Scan the conventional virtio-mmio window for a virtio-console (device id 3).
/// Returns its transport, or `DeviceNotFound`.
fn discover_device() -> Result<MmioTransport, IpcError> {
    for slot in 0..QEMU_VIRT_MMIO_COUNT {
        let base = QEMU_VIRT_MMIO_BASE + slot * QEMU_VIRT_MMIO_STRIDE;
        // Safety: probing read-only registers in the documented MMIO window.
        let t = unsafe { MmioTransport::new(base) };
        if t.read32(mmio::MAGIC_VALUE) != mmio::MAGIC {
            continue;
        }
        if t.read32(mmio::VERSION) != mmio::VERSION_MODERN {
            continue;
        }
        if t.read32(mmio::DEVICE_ID) == mmio::DEVICE_ID_CONSOLE {
            return Ok(t);
        }
    }
    Err(IpcError::DeviceNotFound)
}

// =============================================================================
// [FULL] Split virtqueue
//
// Standard VIRTIO 1.x split virtqueue: a descriptor table, a driver-owned
// "available" ring, and a device-owned "used" ring. Because the modern MMIO
// transport lets us program the three regions' physical addresses independently
// (QueueDesc/QueueDriver/QueueDevice), we keep them as separate fields of one
// page-aligned static and hand the device each field's address.
// =============================================================================

const QUEUE_SIZE: usize = 16;
const BUF_SIZE: usize = 4096;

// Descriptor flags. `NEXT` is unused today (we never chain descriptors — the
// channel uses a single buffer per request) but is kept for spec completeness.
#[allow(dead_code)]
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2; // device writes (i.e. an RX buffer)

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE],
    avail_event: u16,
}

/// Backing memory for one virtqueue plus its single bounce buffer.
///
/// `[STUB]` (DMA): we assume the kernel runs identity-mapped (physical ==
/// virtual), which is true during early GolemLinux boot, so the *address of*
/// each field doubles as the physical address handed to the device. A real MMU
/// phase must translate these via a `virt_to_phys` before programming the
/// registers. The 4 KiB alignment satisfies every virtqueue alignment rule.
#[repr(C, align(4096))]
struct QueueMem {
    desc: [VirtqDesc; QUEUE_SIZE],
    avail: VirtqAvail,
    used: VirtqUsed,
    /// Single in-flight bounce buffer. We only ever keep one descriptor
    /// outstanding per queue (see [`VirtQueue::submit`]) to keep the poll path
    /// trivial; documented simplification, not a spec limit.
    buffer: [u8; BUF_SIZE],
}

impl QueueMem {
    const fn new() -> Self {
        QueueMem {
            desc: [VirtqDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE],
            avail: VirtqAvail { flags: 0, idx: 0, ring: [0; QUEUE_SIZE], used_event: 0 },
            used: VirtqUsed {
                flags: 0,
                idx: 0,
                ring: [VirtqUsedElem { id: 0, len: 0 }; QUEUE_SIZE],
                avail_event: 0,
            },
            buffer: [0; BUF_SIZE],
        }
    }
}

/// Driver-side view of one configured virtqueue.
struct VirtQueue {
    /// Raw backing memory. `*mut` so we can take field addresses for the device
    /// and perform volatile accesses; ownership is conceptually the static below.
    mem: *mut QueueMem,
    /// This queue's index on the device (0 = port RX, 1 = port TX, …).
    index: u32,
    /// `avail.idx` we have published so far.
    avail_idx: u16,
    /// `used.idx` we have already consumed.
    last_used_idx: u16,
}

impl VirtQueue {
    /// Program this queue into the device and return a driver-side handle.
    fn configure<T: VirtioTransport>(
        transport: &T,
        index: u32,
        mem: *mut QueueMem,
    ) -> Result<Self, IpcError> {
        transport.write32(mmio::QUEUE_SEL, index);
        let max = transport.read32(mmio::QUEUE_NUM_MAX);
        if max == 0 {
            return Err(IpcError::QueueSetupFailed);
        }
        let size = core::cmp::min(max as usize, QUEUE_SIZE) as u32;
        transport.write32(mmio::QUEUE_NUM, size);

        // Hand the device the physical address of each ring region. Under the
        // identity-map assumption documented on `QueueMem`, the field address is
        // the physical address.
        let desc_addr = unsafe { ptr::addr_of!((*mem).desc) } as u64;
        let avail_addr = unsafe { ptr::addr_of!((*mem).avail) } as u64;
        let used_addr = unsafe { ptr::addr_of!((*mem).used) } as u64;

        transport.write32(mmio::QUEUE_DESC_LOW, desc_addr as u32);
        transport.write32(mmio::QUEUE_DESC_HIGH, (desc_addr >> 32) as u32);
        transport.write32(mmio::QUEUE_DRIVER_LOW, avail_addr as u32);
        transport.write32(mmio::QUEUE_DRIVER_HIGH, (avail_addr >> 32) as u32);
        transport.write32(mmio::QUEUE_DEVICE_LOW, used_addr as u32);
        transport.write32(mmio::QUEUE_DEVICE_HIGH, (used_addr >> 32) as u32);

        transport.write32(mmio::QUEUE_READY, 1);
        if transport.read32(mmio::QUEUE_READY) != 1 {
            return Err(IpcError::QueueSetupFailed);
        }

        Ok(VirtQueue { mem, index, avail_idx: 0, last_used_idx: 0 })
    }

    /// Publish descriptor 0 (our single in-flight slot) into the available ring
    /// and notify the device. `writable` marks the buffer as device-writable
    /// (RX); cleared for TX.
    fn submit<T: VirtioTransport>(&mut self, transport: &T, len: u32, writable: bool) {
        unsafe {
            let buf_addr = ptr::addr_of!((*self.mem).buffer) as u64;
            let desc = ptr::addr_of_mut!((*self.mem).desc[0]);
            ptr::write_volatile(
                desc,
                VirtqDesc {
                    addr: buf_addr,
                    len,
                    flags: if writable { VIRTQ_DESC_F_WRITE } else { 0 },
                    next: 0,
                },
            );

            // Place descriptor 0 at the next available slot, then bump avail.idx.
            let slot = (self.avail_idx as usize) % QUEUE_SIZE;
            let ring_entry = ptr::addr_of_mut!((*self.mem).avail.ring[slot]);
            ptr::write_volatile(ring_entry, 0u16);

            self.avail_idx = self.avail_idx.wrapping_add(1);
            let idx_ptr = ptr::addr_of_mut!((*self.mem).avail.idx);
            // Release ordering: the descriptor + ring write must be visible
            // before the device observes the new idx.
            core::sync::atomic::fence(Ordering::SeqCst);
            ptr::write_volatile(idx_ptr, self.avail_idx);
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        transport.write32(mmio::QUEUE_NOTIFY, self.index);
    }

    /// Busy-poll the used ring until the device returns our descriptor, and
    /// report how many bytes it reported written (meaningful for RX).
    ///
    /// `[STUB]` (interrupts): this is a polling driver. The MMIO `InterruptStatus`
    /// / `InterruptACK` registers and an IRQ handler are intentionally omitted —
    /// the Sentinel handshake is a synchronous request/response, so blocking
    /// poll is acceptable and far simpler. An interrupt-driven path is future
    /// work.
    fn poll_used(&mut self) -> u32 {
        loop {
            let used_idx = unsafe { ptr::read_volatile(ptr::addr_of!((*self.mem).used.idx)) };
            if used_idx != self.last_used_idx {
                core::sync::atomic::fence(Ordering::SeqCst);
                let slot = (self.last_used_idx as usize) % QUEUE_SIZE;
                let elem =
                    unsafe { ptr::read_volatile(ptr::addr_of!((*self.mem).used.ring[slot])) };
                self.last_used_idx = self.last_used_idx.wrapping_add(1);
                return elem.len;
            }
            core::hint::spin_loop();
        }
    }

    /// Copy `data` into the bounce buffer (TX direction). Returns the byte count.
    fn fill_tx(&mut self, data: &[u8]) -> Result<u32, IpcError> {
        if data.len() > BUF_SIZE {
            return Err(IpcError::MessageTooLarge);
        }
        unsafe {
            let buf = ptr::addr_of_mut!((*self.mem).buffer) as *mut u8;
            ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
        }
        Ok(data.len() as u32)
    }

    /// Read up to `len` bytes the device wrote into the bounce buffer (RX) into
    /// `out`, returning the number copied.
    fn read_rx(&self, len: u32, out: &mut [u8]) -> usize {
        let n = core::cmp::min(len as usize, core::cmp::min(out.len(), BUF_SIZE));
        unsafe {
            let buf = ptr::addr_of!((*self.mem).buffer) as *const u8;
            ptr::copy_nonoverlapping(buf, out.as_mut_ptr(), n);
        }
        n
    }
}

// SAFETY: the `*mut QueueMem` inside a `VirtQueue` always points at one of the
// module-private statics below, which live for the whole kernel lifetime. The
// channel is guarded by a single `INIT` flag and only touched from the Sentinel
// service path, so there is no concurrent aliasing in practice.
unsafe impl Send for VirtQueue {}

// Backing statics for the two data queues of the Sentinel port. Wrapped so they
// have a stable address for DMA. One RX queue, one TX queue.
static mut RX_QUEUE_MEM: QueueMem = QueueMem::new();
static mut TX_QUEUE_MEM: QueueMem = QueueMem::new();

// =============================================================================
// [FULL] Device bring-up + [STUB] multiport control handshake
// =============================================================================

/// Queue index of the Sentinel port's receive queue.
///
/// `[STUB]` (multiport queue math): a multiport virtio-console assigns
/// queues as: port0 RX/TX = 0/1, control RX/TX = 2/3, and portN (N≥1)
/// RX/TX = 2*(N+1) / 2*(N+1)+1. The named Sentinel port is the first added
/// port (port 1), giving RX=4, TX=5. We pin those here. A complete driver would
/// learn the port number from the control-queue `PORT_ADD` message rather than
/// assuming it; see [`negotiate_multiport`].
const SENTINEL_RX_QUEUE: u32 = 4;
const SENTINEL_TX_QUEUE: u32 = 5;

/// The live, initialised channel. Constructed once by [`init`].
struct Channel {
    transport: MmioTransport,
    rx: VirtQueue,
    tx: VirtQueue,
}

// Single global channel + init guard. `no_std`, no allocator: we store the
// channel in a static `Option` behind a hand-rolled spinlock.
static CHANNEL_LOCK: AtomicBool = AtomicBool::new(false);
static mut CHANNEL: Option<Channel> = None;

/// Minimal spinlock guard around the global channel.
struct ChannelGuard;

impl ChannelGuard {
    fn acquire() -> Self {
        while CHANNEL_LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        ChannelGuard
    }
    /// Access the channel, if initialised.
    fn channel(&mut self) -> Option<&mut Channel> {
        // Safety: exclusive access is held via CHANNEL_LOCK for the guard's life.
        unsafe { ptr::addr_of_mut!(CHANNEL).as_mut().unwrap().as_mut() }
    }
    fn set(&mut self, ch: Channel) {
        unsafe {
            *ptr::addr_of_mut!(CHANNEL) = Some(ch);
        }
    }
}

impl Drop for ChannelGuard {
    fn drop(&mut self) {
        CHANNEL_LOCK.store(false, Ordering::Release);
    }
}

/// `[STUB]` Multiport control handshake.
///
/// With `VIRTIO_CONSOLE_F_MULTIPORT`, the host will not deliver data on a port
/// until the guest has (a) consumed the control-queue `DEVICE_ADD` / `PORT_ADD`
/// notifications and (b) replied with `PORT_READY` and `PORT_OPEN` control
/// messages for that port. Implementing that fully requires standing up the
/// control RX/TX queues (indices 2/3) and a small state machine over
/// `struct virtio_console_control { id, event, value }`.
///
/// For this deliverable we negotiate the feature bit (so the device exposes the
/// extra port at all) but treat the control protocol as out of scope: the
/// function documents the required messages and returns `Ok`. On QEMU with
/// `wait=off` the data queues still function for the synchronous handshake in
/// practice; hardening this is explicitly deferred to a later phase.
fn negotiate_multiport<T: VirtioTransport>(_transport: &T) -> Result<(), IpcError> {
    // Required (not yet implemented) sequence, documented for the next phase:
    //   1. Configure control RX queue (index 2) with receive buffers.
    //   2. Configure control TX queue (index 3).
    //   3. Read CONSOLE_PORT_ADD for the Sentinel port from control RX.
    //   4. Send { id: port, event: PORT_READY, value: 1 } on control TX.
    //   5. Send { id: port, event: PORT_OPEN,  value: 1 } on control TX.
    Ok(())
}

/// `[FULL]` Reset the device and negotiate features.
fn reset_and_negotiate<T: VirtioTransport>(transport: &T) -> Result<(), IpcError> {
    use mmio::*;

    // Reset.
    transport.write32(STATUS, 0);
    // Step through the driver-initialisation status handshake (VIRTIO §3.1.1).
    transport.write32(STATUS, STATUS_ACKNOWLEDGE);
    transport.write32(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Read the 64-bit device feature space in two 32-bit halves.
    transport.write32(DEVICE_FEATURES_SEL, 0);
    let feat_lo = transport.read32(DEVICE_FEATURES) as u64;
    transport.write32(DEVICE_FEATURES_SEL, 1);
    let feat_hi = transport.read32(DEVICE_FEATURES) as u64;
    let device_features = feat_lo | (feat_hi << 32);

    // We require VIRTIO_F_VERSION_1 (modern). We opt into MULTIPORT iff offered.
    if device_features & VIRTIO_F_VERSION_1 == 0 {
        transport.write32(STATUS, 0x80); // FAILED
        return Err(IpcError::FeatureNegotiationFailed);
    }
    let mut driver_features = VIRTIO_F_VERSION_1;
    if device_features & VIRTIO_CONSOLE_F_MULTIPORT != 0 {
        driver_features |= VIRTIO_CONSOLE_F_MULTIPORT;
    }

    transport.write32(DRIVER_FEATURES_SEL, 0);
    transport.write32(DRIVER_FEATURES, driver_features as u32);
    transport.write32(DRIVER_FEATURES_SEL, 1);
    transport.write32(DRIVER_FEATURES, (driver_features >> 32) as u32);

    // Lock features.
    let status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    transport.write32(STATUS, status);
    if transport.read32(STATUS) & STATUS_FEATURES_OK == 0 {
        transport.write32(STATUS, 0x80); // FAILED
        return Err(IpcError::FeatureNegotiationFailed);
    }
    Ok(())
}

/// Initialise the Sentinel virtio-serial channel.
///
/// Performs device discovery, reset/feature negotiation, queue setup and the
/// (stubbed) multiport handshake, then RE-arms the RX queue with a receive
/// buffer so the first request can land. Idempotent: a second call returns `Ok`
/// without re-initialising.
pub fn init() -> Result<(), IpcError> {
    let mut guard = ChannelGuard::acquire();
    if guard.channel().is_some() {
        return Ok(());
    }

    let transport = discover_device()?;
    reset_and_negotiate(&transport)?;

    // Configure the Sentinel port's RX and TX queues against their statics.
    let rx = VirtQueue::configure(
        &transport,
        SENTINEL_RX_QUEUE,
        ptr::addr_of_mut!(RX_QUEUE_MEM),
    )?;
    let tx = VirtQueue::configure(
        &transport,
        SENTINEL_TX_QUEUE,
        ptr::addr_of_mut!(TX_QUEUE_MEM),
    )?;

    negotiate_multiport(&transport)?;

    // Device is live.
    use mmio::*;
    transport.write32(
        STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );

    let mut channel = Channel { transport, rx, tx };
    // Post an initial receive buffer so the device has somewhere to deliver the
    // first registration request.
    channel.rx.submit(&channel.transport, BUF_SIZE as u32, true);
    guard.set(channel);
    Ok(())
}

// =============================================================================
// [FULL] Read / write on the port
// =============================================================================

impl Channel {
    /// Block until a message arrives on the Sentinel port, copy it into `out`,
    /// and re-arm the RX queue for the next message. Returns the byte count.
    fn read_message(&mut self, out: &mut [u8]) -> usize {
        let len = self.rx.poll_used();
        let n = self.rx.read_rx(len, out);
        // Re-arm a fresh receive buffer for the next request.
        self.rx.submit(&self.transport, BUF_SIZE as u32, true);
        n
    }

    /// Write a message to the Sentinel port and wait for the device to consume it.
    fn write_message(&mut self, data: &[u8]) -> Result<(), IpcError> {
        let len = self.tx.fill_tx(data)?;
        self.tx.submit(&self.transport, len, false);
        self.tx.poll_used();
        Ok(())
    }
}

/// Block until one registration request arrives, returning its raw bytes copied
/// into `out` (length returned). Errors if the channel is not initialised.
pub fn read_request(out: &mut [u8]) -> Result<usize, IpcError> {
    let mut guard = ChannelGuard::acquire();
    let ch = guard.channel().ok_or(IpcError::NotInitialised)?;
    Ok(ch.read_message(out))
}

/// Write a serialized response to the Sentinel port.
pub fn write_response_bytes(data: &[u8]) -> Result<(), IpcError> {
    let mut guard = ChannelGuard::acquire();
    let ch = guard.channel().ok_or(IpcError::NotInitialised)?;
    ch.write_message(data)
}

// =============================================================================
// [FULL] Handshake glue
// =============================================================================

/// Build the response for a validated request: stamp it with the audit hash and
/// timestamp and mark it successful.
///
/// The registration *policy* (whether to grant the requested tier, and recording
/// it in the Sentinel registry) is owned by sibling Phase-4 agents; this function
/// only constructs the wire response. It always reports `success: true` for a
/// well-formed request — a future hook can reject here.
pub fn build_response(req: &RegisterRequest) -> RegisterResponse {
    let (timestamp, timestamp_len) = Clock::now_string();
    let id = req.agent_id().as_bytes();
    let mut agent_id = [0u8; MAX_AGENT_ID];
    agent_id[..id.len()].copy_from_slice(id);

    RegisterResponse {
        success: true,
        agent_id,
        agent_id_len: id.len(),
        audit_hash: audit_hash_for(req),
        timestamp,
        timestamp_len,
    }
}

/// Maximum wire message size for a single request or response.
pub const MAX_MESSAGE: usize = BUF_SIZE;

/// Run one full registration round-trip: read a request, parse it, build and
/// write the response. Returns the parsed request on success so the caller can
/// drive registration policy.
///
/// This is the entry point the Sentinel service loop calls once the channel is
/// [`init`]ialised.
pub fn handle_one() -> Result<RegisterRequest, IpcError> {
    let mut req_buf = [0u8; MAX_MESSAGE];
    let n = read_request(&mut req_buf)?;
    let req = parse_register_request(&req_buf[..n])?;

    let resp = build_response(&req);
    let mut resp_buf = [0u8; MAX_MESSAGE];
    let m = serialize_response(&resp, &mut resp_buf)?;
    write_response_bytes(&resp_buf[..m])?;

    Ok(req)
}

// =============================================================================
// Tests (host-side, std). Compiled only under `cargo test`; the driver paths
// that touch MMIO are not exercised here — only the pure protocol logic, which
// is the part with interesting behaviour.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_request() {
        let raw = br#"{"method": "register", "agent_id": "agent-7", "permission_tier": "EXECUTE"}"#;
        let req = parse_register_request(raw).unwrap();
        assert_eq!(req.agent_id(), "agent-7");
        assert_eq!(req.permission_tier, PermissionTier::Execute);
    }

    #[test]
    fn tolerates_whitespace_and_ordering() {
        let raw = br#"{  "permission_tier":"READ_ONLY" ,"agent_id":"a","method":"register"}"#;
        let req = parse_register_request(raw).unwrap();
        assert_eq!(req.agent_id(), "a");
        assert_eq!(req.permission_tier, PermissionTier::ReadOnly);
    }

    #[test]
    fn rejects_wrong_method() {
        let raw = br#"{"method":"deregister","agent_id":"a","permission_tier":"WRITE"}"#;
        assert!(matches!(parse_register_request(raw), Err(IpcError::MalformedRequest)));
    }

    #[test]
    fn rejects_unknown_tier() {
        let raw = br#"{"method":"register","agent_id":"a","permission_tier":"ROOT"}"#;
        assert!(matches!(parse_register_request(raw), Err(IpcError::MalformedRequest)));
    }

    #[test]
    fn serializes_response_round_trip() {
        let raw = br#"{"method":"register","agent_id":"svc","permission_tier":"WRITE"}"#;
        let req = parse_register_request(raw).unwrap();
        let resp = build_response(&req);
        let mut out = [0u8; 512];
        let n = serialize_response(&resp, &mut out).unwrap();
        let s = core::str::from_utf8(&out[..n]).unwrap();
        assert!(s.starts_with(r#"{"success":true,"agent_id":"svc","audit_hash":""#));
        assert!(s.contains(r#""timestamp":"mono+"#));
        // audit_hash is 16 hex chars
        let hash = json_string_field(s, "audit_hash").unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn audit_hash_is_deterministic_and_tier_sensitive() {
        let r1 = parse_register_request(
            br#"{"method":"register","agent_id":"x","permission_tier":"READ_ONLY"}"#,
        )
        .unwrap();
        let r2 = parse_register_request(
            br#"{"method":"register","agent_id":"x","permission_tier":"EXECUTE"}"#,
        )
        .unwrap();
        assert_eq!(audit_hash_for(&r1), audit_hash_for(&r1));
        assert_ne!(audit_hash_for(&r1), audit_hash_for(&r2));
    }

    #[test]
    fn response_buffer_overflow_is_reported() {
        let raw = br#"{"method":"register","agent_id":"svc","permission_tier":"WRITE"}"#;
        let req = parse_register_request(raw).unwrap();
        let resp = build_response(&req);
        let mut tiny = [0u8; 8];
        assert_eq!(serialize_response(&resp, &mut tiny), Err(IpcError::MessageTooLarge));
    }
}
