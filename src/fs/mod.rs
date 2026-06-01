// SPDX-License-Identifier: Proprietary
// Copyright (c) 2026 TrueSystems LLC. All rights reserved.
//
// Filesystem layer — public interface.
//
// This module is the only surface the rest of the kernel sees. Sub-modules
// (`vfs`, `ramfs`) are public for direct access where it is genuinely
// useful (e.g. a driver that wants `FileSystem` trait items), but the
// curated re-exports below are the recommended path.
//
// Adding a new filesystem driver:
//   1. Add a sibling module, e.g. `pub mod extfs;`
//   2. Implement `vfs::FileSystem` for the driver's main type.
//   3. Re-export the type below if it is meant to be a first-class part
//      of the public surface.
//
// See `README.md` next to this file for the full design rationale.
//
// Bring-up (Phase 2): the kernel reaches the filesystem through this module's
// global [`VFS`] singleton. Call [`init`] exactly once from `kernel_main`
// AFTER the heap is online (this module allocates immediately — a ramfs root
// plus its directory skeleton). See `README.md` ("Kernel integration") for the
// hard ordering contract and the crate-root requirements (`#![no_std]` and
// `extern crate alloc;`) that this subsystem depends on but does not own.

// NOTE: `#![no_std]` is a *crate-level* attribute; the one that actually takes
// effect is in the crate root (`src/main.rs`, owned by the integration agent).
// We restate it here, guarded for host `cargo test`, to declare this module's
// intent and to stay consistent with the other subsystems. rustc emits a
// harmless `unused_attributes` warning for it in a non-root module.
#![cfg_attr(not(test), no_std)]
#![allow(clippy::module_inception)]

pub mod ramfs;
pub mod vfs;

pub use ramfs::RamFs;
pub use vfs::{
    DirEntry, File, FileSystem, FileType, FsError, FsResult, InodeNum, Metadata, Mode, OpenFlags,
    SeekFrom, Vfs,
};

use core::sync::atomic::{AtomicBool, Ordering};

/// The kernel's single filesystem instance.
///
/// Owned here so the rest of the kernel reaches the filesystem only through
/// `fs::VFS` (mirroring how the sentinel subsystem exposes `SENTINEL`). It is
/// constructed at compile time — `Vfs::new()` is `const` — and is empty until
/// [`init`] mounts the root.
pub static VFS: Vfs = Vfs::new();

/// Bring-up guard so [`init`] is idempotent.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Baseline directory skeleton created on the fresh ramfs root so that early
/// userspace (and later mounts such as a real `devfs`) have the conventional
/// mount points and a writable scratch dir to land on. These are pure ramfs
/// operations — no other subsystem is touched.
const SKELETON: [(&str, Mode); 4] = [
    ("/dev", 0o755),
    ("/proc", 0o555),
    ("/sys", 0o555),
    ("/tmp", 0o1777),
];

/// Filesystem bring-up. Mounts RamFs at `/` and creates the baseline
/// directory skeleton, leaving a usable root filesystem in place before any
/// userspace process runs.
///
/// Call exactly once from `kernel_main`. Idempotent: a second call is a no-op.
///
/// # Ordering dependency
///
/// The heap MUST be initialized first (the memory agent's `memory::init`).
/// This function allocates immediately — the ramfs inode table and every
/// directory below live on the heap. Calling it before the global allocator
/// is registered will fault. Per the agreed `kernel_main` order this runs as
/// step 3, after `sentinel::init` and `memory::init`.
///
/// # Panics
///
/// On a freshly-constructed [`VFS`] both the mount and the skeleton creation
/// are structurally infallible. A failure therefore means the mount table was
/// corrupted before bring-up — unrecoverable — so we surface it loudly rather
/// than booting onto a broken root.
pub fn init() {
    if INITIALIZED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // already initialized — bring-up is idempotent
    }

    // Mount the in-memory root. `RamFs::new()` returns `Arc<RamFs>`, which
    // coerces to `Arc<dyn FileSystem>` at the call.
    let root = RamFs::new();
    VFS.mount("/", root)
        .expect("fs::init: failed to mount ramfs at / (heap initialized?)");

    for (path, mode) in SKELETON {
        VFS.mkdir(path, mode)
            .expect("fs::init: failed to create baseline directory on ramfs root");
    }
}

/// Whether [`init`] has completed. Lets other subsystems assert ordering.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}
