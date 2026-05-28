//! Immutable, append-only, SHA-256-hashed kernel audit log.
//!
//! Every consequential action against Sentinel — a handshake completion, a
//! permission tier change, a degradation tier crossing, an attempted query
//! from an agent context — produces exactly one entry in this log. Entries
//! are chained: each new entry's hash digests its predecessor's hash, so
//! tampering with any earlier entry breaks every subsequent hash. The chain
//! property is the system guarantee Golem promises: *every action has a hash*.
//!
//! The data model mirrors `sentinel-core::audit` so a userspace verifier can
//! consume both feeds with one parser. Kernel-side concessions:
//!
//! * `no_std + alloc` — no tokio, no std::sync, no filesystem I/O here. The
//!   log lives in kernel memory; persistence is a separate concern handled by
//!   the audit-flush worker (out of scope for this module).
//! * A minimal embedded SHA-256 (FIPS 180-4). We deliberately do not pull in
//!   `sha2` or `ring`: the audit primitive must not have a transitive
//!   dependency surface, since the audit log is the thing we use to detect
//!   tampering with everything else.
//! * Synchronization via [`SpinLock`](crate::sentinel::SpinLock). The kernel
//!   has no async runtime — a contended record is a few hundred cycles of
//!   spin, which is cheaper than the syscall that triggered it.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;

use crate::sentinel::SpinLock;

/// One row in the audit chain. Cloneable so the kernel debugger can grab a
/// snapshot without holding the lock.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub sequence: u64,
    pub timestamp: u64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub hash: String,
    pub prev_hash: String,
}

/// Append-only chain. No `remove`, no `truncate`, no `replace_at` — and there
/// won't be. If you find yourself wanting one of those, the answer is "use a
/// separate retention store, then verify the chain into it."
pub struct AuditTrail {
    inner: SpinLock<AuditInner>,
}

struct AuditInner {
    entries: Vec<AuditEntry>,
    last_hash: String,
    sequence: u64,
}

impl AuditTrail {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(AuditInner {
                entries: Vec::new(),
                last_hash: String::new(),
                sequence: 0,
            }),
        }
    }

    /// Record an action. Returns the entry's hash so the caller can log /
    /// echo it back to the operator without re-reading the chain.
    ///
    /// `timestamp` is passed in (not read from a clock) so the audit module
    /// stays a pure function of its inputs and trivially testable.
    pub fn record(
        &self,
        timestamp: u64,
        actor: &str,
        action: &str,
        target: &str,
    ) -> String {
        let mut g = self.inner.lock();
        g.sequence += 1;
        let seq = g.sequence;
        let prev = if g.last_hash.is_empty() {
            "0".repeat(64)
        } else {
            g.last_hash.clone()
        };

        let payload = format!("{seq}|{timestamp}|{actor}|{action}|{target}|{prev}");
        let hash = sha256_hex(payload.as_bytes());

        let entry = AuditEntry {
            sequence: seq,
            timestamp,
            actor: actor.to_string(),
            action: action.to_string(),
            target: target.to_string(),
            hash: hash.clone(),
            prev_hash: prev,
        };
        g.entries.push(entry);
        g.last_hash = hash.clone();
        hash
    }

    /// Walk the chain from genesis and confirm every link recomputes to its
    /// stored hash. Returns the number of verified entries, or the sequence
    /// number of the first broken link.
    pub fn verify(&self) -> Result<usize, String> {
        let g = self.inner.lock();
        let mut expected_prev = "0".repeat(64);
        for entry in g.entries.iter() {
            if entry.prev_hash != expected_prev {
                return Err(format!(
                    "chain broken at sequence {}: expected prev_hash {expected_prev}, got {}",
                    entry.sequence, entry.prev_hash
                ));
            }
            let payload = format!(
                "{}|{}|{}|{}|{}|{}",
                entry.sequence, entry.timestamp, entry.actor,
                entry.action, entry.target, entry.prev_hash
            );
            let computed = sha256_hex(payload.as_bytes());
            if computed != entry.hash {
                return Err(format!(
                    "hash mismatch at sequence {}: computed {computed}, stored {}",
                    entry.sequence, entry.hash
                ));
            }
            expected_prev = entry.hash.clone();
        }
        Ok(g.entries.len())
    }

    /// Read-only snapshot. The kernel debugger and the Safe Mode export tool
    /// are the only legitimate callers; nothing else should be enumerating
    /// the audit trail at runtime.
    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.inner.lock().entries.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4). Embedded so the audit primitive has no dependencies.
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
    0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H_INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

#[inline(always)]
fn ch(x: u32, y: u32, z: u32) -> u32 { (x & y) ^ (!x & z) }
#[inline(always)]
fn maj(x: u32, y: u32, z: u32) -> u32 { (x & y) ^ (x & z) ^ (y & z) }
#[inline(always)]
fn big_sigma0(x: u32) -> u32 { x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22) }
#[inline(always)]
fn big_sigma1(x: u32) -> u32 { x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25) }
#[inline(always)]
fn small_sigma0(x: u32) -> u32 { x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3) }
#[inline(always)]
fn small_sigma1(x: u32) -> u32 { x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10) }

/// SHA-256 → 64-char lowercase hex. Pure function, no allocations beyond the
/// returned String.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = H_INIT;
    let bit_len = (data.len() as u64) * 8;
    let mut msg: Vec<u8> = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4], block[i * 4 + 1],
                block[i * 4 + 2], block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            w[i] = small_sigma1(w[i - 2])
                .wrapping_add(w[i - 7])
                .wrapping_add(small_sigma0(w[i - 15]))
                .wrapping_add(w[i - 16]);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let t1 = hh
                .wrapping_add(big_sigma1(e))
                .wrapping_add(ch(e, f, g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let t2 = big_sigma0(a).wrapping_add(maj(a, b, c));
            hh = g; g = f; f = e;
            e = d.wrapping_add(t1);
            d = c; c = b; b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for v in h.iter() {
        out.push_str(&format!("{v:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn chain_records_and_verifies() {
        let t = AuditTrail::new();
        t.record(1, "kernel", "handshake", "agent-7");
        t.record(2, "kernel", "tier-cross", "agent-7");
        t.record(3, "kernel", "deregister", "agent-7");
        assert_eq!(t.verify(), Ok(3));
    }

    #[test]
    fn chain_detects_tampering() {
        let t = AuditTrail::new();
        t.record(1, "k", "x", "a");
        t.record(2, "k", "y", "a");
        // Forge entry 1 in-place: the chain must reject the rewrite.
        {
            let mut g = t.inner.lock();
            g.entries[0].action = "FORGED".to_string();
        }
        assert!(t.verify().is_err());
    }

    #[test]
    fn hashes_are_unique_per_entry() {
        let t = AuditTrail::new();
        let h1 = t.record(10, "op", "register", "a1");
        let h2 = t.record(10, "op", "register", "a2");
        let h3 = t.record(10, "op", "register", "a1"); // same actor+action+target as h1
        assert_ne!(h1, h2);
        // h3 differs from h1 because prev_hash has changed even though
        // actor/action/target/timestamp didn't — the chain forces uniqueness.
        assert_ne!(h1, h3);
    }
}
