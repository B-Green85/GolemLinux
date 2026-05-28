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

#![allow(clippy::module_inception)]

pub mod ramfs;
pub mod vfs;

pub use ramfs::RamFs;
pub use vfs::{
    DirEntry, File, FileSystem, FileType, FsError, FsResult, InodeNum, Metadata, Mode, OpenFlags,
    SeekFrom, Vfs,
};
