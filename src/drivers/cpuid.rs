//! CPUID-based CPU identification for the Golem kernel (gkern).
//!
//! `CPUID` is the x86 instruction that reports who the processor is and what it
//! can do. It is **read-only** — it never changes machine state — so it is sound
//! to execute at any point after the CPU is in long mode (which is the only mode
//! the kernel ever runs in). The driver is consumed once at boot, after memory
//! init and before the scheduler, to validate we are on the expected hardware
//! and to document that hardware in the serial boot log.
//!
//! # Design
//!
//! The raw `cpuid` read (privileged-free, but architecture-specific) is kept in
//! one tiny `unsafe` shim, [`raw_cpuid`]. Everything else — turning the four
//! returned registers into a vendor string, a family/model/stepping triple, and
//! a feature set — is **pure decoding logic** that takes the register values as
//! plain `u32`s. That split keeps the interesting code architecture-independent
//! and unit-testable on the (non-x86) build host, while the real hardware path
//! is exercised only by the kernel build for `x86_64-unknown-none`.
//!
//! # `no_std`
//!
//! This file links only `core`. The authoritative `#![no_std]` lives at the
//! crate root (`src/main.rs`, owned by the integration agent); see
//! `mod.rs` for the convention mirror.

use core::fmt;

/// The vendor string we expect to be running on. The Golem target hardware is
/// Intel, so anything other than this is flagged in the boot log.
pub const EXPECTED_VENDOR: &str = "GenuineIntel";

// --- Raw CPUID read --------------------------------------------------------

/// Execute `cpuid` for `leaf` (sub-leaf / ECX = 0) and return `(eax, ebx, ecx,
/// edx)`.
///
/// On `x86_64` this defers to `core::arch::x86_64::__cpuid`, which is the
/// correct way to issue the instruction: a hand-rolled `asm!("cpuid")` would
/// clobber `rbx`, which LLVM reserves, so the intrinsic exists precisely to
/// save/restore it for us.
///
/// SAFETY: `cpuid` is unprivileged and has no memory or ordering effects, so it
/// is sound to call any time the CPU is in long mode — which, for this kernel,
/// is always. The intrinsic is `unsafe` only because it is a target intrinsic.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn raw_cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    // SAFETY: see the doc comment — `cpuid` is read-only and always legal here.
    let r = unsafe { core::arch::x86_64::__cpuid(leaf) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Host-build stub so the decoding logic in this file compiles (and its unit
/// tests run) on the non-x86 build machine. The kernel itself is only ever
/// built for `x86_64-unknown-none`, where the real implementation above is
/// used; this branch never runs on real hardware.
#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub fn raw_cpuid(_leaf: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}

// --- Feature set -----------------------------------------------------------

/// The subset of CPU features the kernel cares about reporting at boot.
///
/// All three live in the standard feature leaf (CPUID leaf 1). SSE/SSE2 are in
/// `EDX`; AVX is in `ECX`. We report only what the spec asks for — presence of
/// these is what tells us the SIMD baseline the rest of the system may assume.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub sse: bool,
    pub sse2: bool,
    pub avx: bool,
}

impl Features {
    /// Bit 25 of leaf-1 `EDX`.
    const SSE_BIT: u32 = 1 << 25;
    /// Bit 26 of leaf-1 `EDX`.
    const SSE2_BIT: u32 = 1 << 26;
    /// Bit 28 of leaf-1 `ECX`.
    const AVX_BIT: u32 = 1 << 28;

    /// Decode the SSE/SSE2/AVX flags from the leaf-1 `ECX`/`EDX` register pair.
    pub fn decode(ecx: u32, edx: u32) -> Self {
        Features {
            sse: edx & Self::SSE_BIT != 0,
            sse2: edx & Self::SSE2_BIT != 0,
            avx: ecx & Self::AVX_BIT != 0,
        }
    }
}

// --- Decoded CPU identity --------------------------------------------------

/// Everything we read out of CPUID at boot.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    /// Highest standard CPUID leaf the CPU supports (leaf 0, `EAX`).
    pub max_leaf: u32,
    /// 12-byte vendor identification string (not NUL-terminated).
    pub vendor: [u8; 12],
    /// Effective family, after folding in the extended-family field.
    pub family: u32,
    /// Effective model, after folding in the extended-model field.
    pub model: u32,
    /// Processor stepping ID.
    pub stepping: u32,
    /// Reported SIMD feature flags.
    pub features: Features,
}

impl CpuInfo {
    /// Decode the vendor string from the leaf-0 register triple.
    ///
    /// The 12 vendor bytes are returned across `EBX`, then `EDX`, then `ECX`
    /// (note the order — `ECX` is *last*, not second), each register holding 4
    /// ASCII bytes little-endian. For an Intel part this spells `GenuineIntel`.
    pub fn decode_vendor(ebx: u32, ecx: u32, edx: u32) -> [u8; 12] {
        let mut v = [0u8; 12];
        v[0..4].copy_from_slice(&ebx.to_le_bytes());
        v[4..8].copy_from_slice(&edx.to_le_bytes());
        v[8..12].copy_from_slice(&ecx.to_le_bytes());
        v
    }

    /// Decode family / model / stepping from the leaf-1 `EAX` "version
    /// information" word, applying the standard x86 extended-field rules:
    ///
    /// * The displayed **family** is the 4-bit base family, *plus* the 8-bit
    ///   extended family when the base family is `0xF`.
    /// * The displayed **model** is the 4-bit base model, with the 4-bit
    ///   extended model shifted in as the high nibble when the base family is
    ///   `0x6` or `0xF` (the cases Intel/AMD defined the extension for).
    pub fn decode_version(eax: u32) -> (u32, u32, u32) {
        let stepping = eax & 0xF;
        let base_model = (eax >> 4) & 0xF;
        let base_family = (eax >> 8) & 0xF;
        let ext_model = (eax >> 16) & 0xF;
        let ext_family = (eax >> 20) & 0xFF;

        let family = if base_family == 0xF {
            base_family + ext_family
        } else {
            base_family
        };
        let model = if base_family == 0x6 || base_family == 0xF {
            (ext_model << 4) | base_model
        } else {
            base_model
        };
        (family, model, stepping)
    }

    /// The vendor field as a `&str`. CPUID vendor bytes are always ASCII, so
    /// this is normally `Some`; it is `None` only if a bogus/zeroed value
    /// somehow contained a non-UTF-8 byte (e.g. the host-build stub).
    pub fn vendor_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.vendor).ok()
    }

    /// Whether the detected vendor matches [`EXPECTED_VENDOR`].
    pub fn vendor_matches_expected(&self) -> bool {
        self.vendor == EXPECTED_VENDOR.as_bytes()
    }
}

/// Read and decode the CPU's identity via CPUID.
///
/// This is the one entry point that actually touches the hardware. It reads the
/// vendor leaf (0) and, if the CPU advertises it, the version/feature leaf (1),
/// then hands the raw registers to the pure decoders above.
pub fn detect() -> CpuInfo {
    let (max_leaf, ebx0, ecx0, edx0) = raw_cpuid(0);
    let vendor = CpuInfo::decode_vendor(ebx0, ecx0, edx0);

    // Leaf 1 carries version info + the SSE/SSE2/AVX flags. Only read it if the
    // CPU says it exists (max_leaf >= 1); otherwise leave the fields at their
    // zero/empty defaults rather than reading an undefined leaf.
    let (family, model, stepping, features) = if max_leaf >= 1 {
        let (eax1, _ebx1, ecx1, edx1) = raw_cpuid(1);
        let (family, model, stepping) = CpuInfo::decode_version(eax1);
        (family, model, stepping, Features::decode(ecx1, edx1))
    } else {
        (0, 0, 0, Features::default())
    };

    CpuInfo {
        max_leaf,
        vendor,
        family,
        model,
        stepping,
        features,
    }
}

// --- Boot-log formatting ---------------------------------------------------

impl fmt::Display for CpuInfo {
    /// Render the multi-line block written to the serial boot log. Each line is
    /// prefixed to nest under the `cpuid:` heading the driver's `init` emits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vendor = self.vendor_str().unwrap_or("<non-ascii>");
        writeln!(f, "    vendor: {}", vendor)?;
        if self.vendor_matches_expected() {
            writeln!(f, "    vendor check: ok (expected {})", EXPECTED_VENDOR)?;
        } else {
            writeln!(
                f,
                "    vendor check: MISMATCH (expected {})",
                EXPECTED_VENDOR
            )?;
        }
        writeln!(
            f,
            "    family/model/stepping: {}/{}/{} (0x{:x}/0x{:x}/0x{:x})",
            self.family,
            self.model,
            self.stepping,
            self.family,
            self.model,
            self.stepping
        )?;
        writeln!(
            f,
            "    features: SSE={} SSE2={} AVX={}",
            yn(self.features.sse),
            yn(self.features.sse2),
            yn(self.features.avx)
        )?;
        write!(f, "    max cpuid leaf: 0x{:x}", self.max_leaf)
    }
}

/// Compact `yes`/`no` for the feature line.
fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real leaf-0 registers from an Intel part: "GenuineIntel".
    // EBX = "Genu", EDX = "ineI", ECX = "ntel".
    const INTEL_EBX: u32 = u32::from_le_bytes(*b"Genu");
    const INTEL_EDX: u32 = u32::from_le_bytes(*b"ineI");
    const INTEL_ECX: u32 = u32::from_le_bytes(*b"ntel");

    #[test]
    fn vendor_decodes_genuine_intel() {
        let v = CpuInfo::decode_vendor(INTEL_EBX, INTEL_ECX, INTEL_EDX);
        assert_eq!(&v, b"GenuineIntel");
        assert_eq!(core::str::from_utf8(&v).unwrap(), EXPECTED_VENDOR);
    }

    #[test]
    fn vendor_matches_expected_flag() {
        let info = CpuInfo {
            max_leaf: 1,
            vendor: *b"GenuineIntel",
            family: 0,
            model: 0,
            stepping: 0,
            features: Features::default(),
        };
        assert!(info.vendor_matches_expected());

        let amd = CpuInfo {
            vendor: *b"AuthenticAMD",
            ..info
        };
        assert!(!amd.vendor_matches_expected());
    }

    #[test]
    fn version_simple_family_six() {
        // base family 0x6, base model 0xA, stepping 0x5, no extended fields.
        // family stays 6; model stays 0xA (ext model nibble is 0).
        let eax = (0x6 << 8) | (0xA << 4) | 0x5;
        let (family, model, stepping) = CpuInfo::decode_version(eax);
        assert_eq!(family, 0x6);
        assert_eq!(model, 0xA);
        assert_eq!(stepping, 0x5);
    }

    #[test]
    fn version_extended_model_folds_in_for_family_six() {
        // base family 0x6 → extended model nibble becomes the high nibble.
        // ext_model = 0x3, base_model = 0xE → model 0x3E.
        let eax = (0x3 << 16) | (0x6 << 8) | (0xE << 4) | 0x2;
        let (family, model, stepping) = CpuInfo::decode_version(eax);
        assert_eq!(family, 0x6);
        assert_eq!(model, 0x3E);
        assert_eq!(stepping, 0x2);
    }

    #[test]
    fn version_extended_family_adds_for_family_fifteen() {
        // base family 0xF → extended family (0x02) is added: 0xF + 0x2 = 0x11.
        // base family is 0xF so extended model also folds in: ext 0x1, base 0x7
        // → model 0x17.
        let eax = (0x02 << 20) | (0x1 << 16) | (0xF << 8) | (0x7 << 4) | 0x1;
        let (family, model, stepping) = CpuInfo::decode_version(eax);
        assert_eq!(family, 0xF + 0x2);
        assert_eq!(model, 0x17);
        assert_eq!(stepping, 0x1);
    }

    #[test]
    fn features_decode_individual_bits() {
        let none = Features::decode(0, 0);
        assert!(!none.sse && !none.sse2 && !none.avx);

        let sse_only = Features::decode(0, 1 << 25);
        assert!(sse_only.sse && !sse_only.sse2 && !sse_only.avx);

        let sse2_only = Features::decode(0, 1 << 26);
        assert!(!sse2_only.sse && sse2_only.sse2 && !sse2_only.avx);

        let avx_only = Features::decode(1 << 28, 0);
        assert!(!avx_only.sse && !avx_only.sse2 && avx_only.avx);

        let all = Features::decode(1 << 28, (1 << 25) | (1 << 26));
        assert!(all.sse && all.sse2 && all.avx);
    }
}
