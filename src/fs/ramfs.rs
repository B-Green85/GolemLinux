// SPDX-License-Identifier: Proprietary
// Copyright (c) 2026 TrueSystems LLC. All rights reserved.
//
// RamFs — the in-memory bootstrap filesystem.
//
// This is the filesystem the kernel mounts at `/` before any block driver
// is available. Every node lives in heap memory and is gone at reboot. It
// is intentionally simple — its job is to let the kernel boot, hold
// `/dev`, `/proc`, `/sys` mount points, and stage early-boot binaries.
//
// Storage layout:
//   - `BTreeMap<InodeNum, Node>` is the inode table. `BTreeMap` avoids
//     pulling in `hashbrown` and gives deterministic ordering for readdir.
//   - Each directory stores its own `BTreeMap<String, InodeNum>` of named
//     children. Directories also remember their parent inode so the VFS
//     can resolve `..` via `lookup(parent, "..")`.
//   - Files are `Vec<u8>`. Sparse files are not supported — a write past
//     EOF zero-extends. That matches Linux semantics and is fine for ramfs.
//
// Concurrency: one `spin::Mutex` around the whole inode table. Coarse, but
// correct, and ramfs is not a hot path. If profiling later shows
// contention, the table can be sharded or moved behind an `RwLock`.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use super::vfs::{
    DirEntry, FileSystem, FileType, FsError, FsResult, InodeNum, Metadata, Mode,
};

/// The root inode number. Convention from Unix: root is inode 1, never 0.
/// Inode 0 is reserved as "no such inode" by some external tooling.
const ROOT_INODE: InodeNum = 1;

enum NodeKind {
    File(Vec<u8>),
    Directory(BTreeMap<String, InodeNum>),
    Symlink(String),
}

impl NodeKind {
    fn file_type(&self) -> FileType {
        match self {
            NodeKind::File(_) => FileType::Regular,
            NodeKind::Directory(_) => FileType::Directory,
            NodeKind::Symlink(_) => FileType::Symlink,
        }
    }
}

struct Node {
    /// Parent inode. For the root directory, this is `ROOT_INODE` (itself);
    /// that matches the convention that `/..` resolves to `/`.
    parent: InodeNum,
    kind: NodeKind,
    mode: Mode,
    uid: u32,
    gid: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
}

struct Inner {
    nodes: BTreeMap<InodeNum, Node>,
    next_inode: InodeNum,
}

pub struct RamFs {
    inner: Mutex<Inner>,
}

impl RamFs {
    /// Build an empty ramfs containing only a root directory. Returned as
    /// `Arc` so callers can `vfs.mount("/", RamFs::new())` directly.
    pub fn new() -> Arc<Self> {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                parent: ROOT_INODE,
                kind: NodeKind::Directory(BTreeMap::new()),
                mode: 0o755,
                uid: 0,
                gid: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
            },
        );
        Arc::new(Self {
            inner: Mutex::new(Inner {
                nodes,
                next_inode: ROOT_INODE + 1,
            }),
        })
    }
}

/// Reject `/`, empty, `.`, `..` as new names. Centralized so every
/// name-accepting operation has the same rule set.
fn validate_name(name: &str) -> FsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(FsError::InvalidArgument);
    }
    Ok(())
}

impl FileSystem for RamFs {
    fn name(&self) -> &str {
        "ramfs"
    }

    fn root_inode(&self) -> InodeNum {
        ROOT_INODE
    }

    fn metadata(&self, inode: InodeNum) -> FsResult<Metadata> {
        let inner = self.inner.lock();
        let node = inner.nodes.get(&inode).ok_or(FsError::NotFound)?;
        let size = match &node.kind {
            NodeKind::File(b) => b.len() as u64,
            NodeKind::Directory(d) => d.len() as u64,
            NodeKind::Symlink(s) => s.len() as u64,
        };
        // For directories, the conventional Unix nlink is 2 (self, `.`) plus
        // one for each child subdirectory's `..` entry. We compute this on
        // demand to keep `Node` simpler.
        let nlink = match &node.kind {
            NodeKind::Directory(d) => {
                2 + d
                    .values()
                    .filter(|&&i| {
                        inner
                            .nodes
                            .get(&i)
                            .map_or(false, |n| matches!(n.kind, NodeKind::Directory(_)))
                    })
                    .count() as u32
            }
            _ => 1,
        };
        Ok(Metadata {
            inode,
            file_type: node.kind.file_type(),
            size,
            mode: node.mode,
            uid: node.uid,
            gid: node.gid,
            nlink,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
        })
    }

    fn lookup(&self, parent: InodeNum, name: &str) -> FsResult<InodeNum> {
        let inner = self.inner.lock();
        let node = inner.nodes.get(&parent).ok_or(FsError::NotFound)?;
        match &node.kind {
            NodeKind::Directory(entries) => {
                if name == "." {
                    Ok(parent)
                } else if name == ".." {
                    Ok(node.parent)
                } else {
                    entries.get(name).copied().ok_or(FsError::NotFound)
                }
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    fn readdir(&self, inode: InodeNum) -> FsResult<Vec<DirEntry>> {
        let inner = self.inner.lock();
        let node = inner.nodes.get(&inode).ok_or(FsError::NotFound)?;
        let entries = match &node.kind {
            NodeKind::Directory(e) => e,
            _ => return Err(FsError::NotADirectory),
        };
        let mut out = Vec::with_capacity(entries.len() + 2);
        out.push(DirEntry {
            name: ".".to_string(),
            inode,
            file_type: FileType::Directory,
        });
        out.push(DirEntry {
            name: "..".to_string(),
            inode: node.parent,
            file_type: FileType::Directory,
        });
        for (name, inum) in entries {
            let ft = inner
                .nodes
                .get(inum)
                .map(|n| n.kind.file_type())
                .unwrap_or(FileType::Regular);
            out.push(DirEntry {
                name: name.clone(),
                inode: *inum,
                file_type: ft,
            });
        }
        Ok(out)
    }

    fn read(&self, inode: InodeNum, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        let inner = self.inner.lock();
        let node = inner.nodes.get(&inode).ok_or(FsError::NotFound)?;
        match &node.kind {
            NodeKind::File(data) => {
                let off = offset as usize;
                if off >= data.len() {
                    return Ok(0);
                }
                let n = core::cmp::min(buf.len(), data.len() - off);
                buf[..n].copy_from_slice(&data[off..off + n]);
                Ok(n)
            }
            NodeKind::Directory(_) => Err(FsError::IsADirectory),
            NodeKind::Symlink(_) => Err(FsError::InvalidArgument),
        }
    }

    fn write(&self, inode: InodeNum, offset: u64, buf: &[u8]) -> FsResult<usize> {
        let mut inner = self.inner.lock();
        let node = inner.nodes.get_mut(&inode).ok_or(FsError::NotFound)?;
        match &mut node.kind {
            NodeKind::File(data) => {
                let off = offset as usize;
                let end = off.checked_add(buf.len()).ok_or(FsError::NoSpace)?;
                if data.len() < end {
                    // Zero-extend on sparse writes.
                    data.resize(end, 0);
                }
                data[off..end].copy_from_slice(buf);
                Ok(buf.len())
            }
            NodeKind::Directory(_) => Err(FsError::IsADirectory),
            NodeKind::Symlink(_) => Err(FsError::InvalidArgument),
        }
    }

    fn create(
        &self,
        parent: InodeNum,
        name: &str,
        file_type: FileType,
        mode: Mode,
    ) -> FsResult<InodeNum> {
        validate_name(name)?;
        let mut inner = self.inner.lock();
        // Parent must exist, be a directory, and not already contain `name`.
        {
            let p = inner.nodes.get(&parent).ok_or(FsError::NotFound)?;
            match &p.kind {
                NodeKind::Directory(entries) => {
                    if entries.contains_key(name) {
                        return Err(FsError::AlreadyExists);
                    }
                }
                _ => return Err(FsError::NotADirectory),
            }
        }
        let kind = match file_type {
            FileType::Regular => NodeKind::File(Vec::new()),
            FileType::Directory => NodeKind::Directory(BTreeMap::new()),
            FileType::Symlink => NodeKind::Symlink(String::new()),
            // Device/fifo/socket nodes are filesystem-agnostic stubs that
            // the kernel layers above us interpret. Ramfs does not host
            // them yet.
            _ => return Err(FsError::Unsupported),
        };
        let inum = inner.next_inode;
        inner.next_inode = inner
            .next_inode
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        inner.nodes.insert(
            inum,
            Node {
                parent,
                kind,
                mode,
                uid: 0,
                gid: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
            },
        );
        if let Some(p) = inner.nodes.get_mut(&parent) {
            if let NodeKind::Directory(entries) = &mut p.kind {
                entries.insert(name.to_string(), inum);
            }
        }
        Ok(inum)
    }

    fn unlink(&self, parent: InodeNum, name: &str) -> FsResult<()> {
        let mut inner = self.inner.lock();
        let inum = {
            let p = inner.nodes.get(&parent).ok_or(FsError::NotFound)?;
            match &p.kind {
                NodeKind::Directory(entries) => {
                    *entries.get(name).ok_or(FsError::NotFound)?
                }
                _ => return Err(FsError::NotADirectory),
            }
        };
        // Cannot use unlink to remove a directory.
        let target = inner.nodes.get(&inum).ok_or(FsError::NotFound)?;
        if matches!(target.kind, NodeKind::Directory(_)) {
            return Err(FsError::IsADirectory);
        }
        if let Some(p) = inner.nodes.get_mut(&parent) {
            if let NodeKind::Directory(entries) = &mut p.kind {
                entries.remove(name);
            }
        }
        inner.nodes.remove(&inum);
        Ok(())
    }

    fn rmdir(&self, parent: InodeNum, name: &str) -> FsResult<()> {
        let mut inner = self.inner.lock();
        let inum = {
            let p = inner.nodes.get(&parent).ok_or(FsError::NotFound)?;
            match &p.kind {
                NodeKind::Directory(entries) => {
                    *entries.get(name).ok_or(FsError::NotFound)?
                }
                _ => return Err(FsError::NotADirectory),
            }
        };
        {
            let target = inner.nodes.get(&inum).ok_or(FsError::NotFound)?;
            match &target.kind {
                NodeKind::Directory(entries) => {
                    if !entries.is_empty() {
                        return Err(FsError::NotEmpty);
                    }
                }
                _ => return Err(FsError::NotADirectory),
            }
        }
        if let Some(p) = inner.nodes.get_mut(&parent) {
            if let NodeKind::Directory(entries) = &mut p.kind {
                entries.remove(name);
            }
        }
        inner.nodes.remove(&inum);
        Ok(())
    }

    fn rename(
        &self,
        old_parent: InodeNum,
        old_name: &str,
        new_parent: InodeNum,
        new_name: &str,
    ) -> FsResult<()> {
        validate_name(new_name)?;
        let mut inner = self.inner.lock();

        // Resolve source inode.
        let inum = {
            let op = inner.nodes.get(&old_parent).ok_or(FsError::NotFound)?;
            match &op.kind {
                NodeKind::Directory(entries) => {
                    *entries.get(old_name).ok_or(FsError::NotFound)?
                }
                _ => return Err(FsError::NotADirectory),
            }
        };

        // Destination parent must exist and be a directory.
        let np_is_dir = matches!(
            inner.nodes.get(&new_parent).map(|n| &n.kind),
            Some(NodeKind::Directory(_))
        );
        if !np_is_dir {
            return Err(FsError::NotADirectory);
        }

        // Reject moving a directory into itself or a descendant — would
        // create an orphaned cycle that's unrecoverable in-memory.
        let src_is_dir = matches!(
            inner.nodes.get(&inum).map(|n| &n.kind),
            Some(NodeKind::Directory(_))
        );
        if src_is_dir {
            let mut cur = new_parent;
            loop {
                if cur == inum {
                    return Err(FsError::InvalidArgument);
                }
                let n = inner.nodes.get(&cur).ok_or(FsError::NotFound)?;
                if n.parent == cur {
                    break;
                }
                cur = n.parent;
            }
        }

        // If `new_name` already exists at the destination, handle replace.
        let existing = {
            if let Some(NodeKind::Directory(entries)) =
                inner.nodes.get(&new_parent).map(|n| &n.kind)
            {
                entries.get(new_name).copied()
            } else {
                None
            }
        };
        if let Some(eid) = existing {
            if eid == inum {
                return Ok(());
            }
            let dst_is_dir = matches!(
                inner.nodes.get(&eid).map(|n| &n.kind),
                Some(NodeKind::Directory(_))
            );
            if src_is_dir != dst_is_dir {
                return Err(if dst_is_dir {
                    FsError::IsADirectory
                } else {
                    FsError::NotADirectory
                });
            }
            if dst_is_dir {
                if let Some(NodeKind::Directory(entries)) =
                    inner.nodes.get(&eid).map(|n| &n.kind)
                {
                    if !entries.is_empty() {
                        return Err(FsError::NotEmpty);
                    }
                }
            }
            inner.nodes.remove(&eid);
            if let Some(np) = inner.nodes.get_mut(&new_parent) {
                if let NodeKind::Directory(entries) = &mut np.kind {
                    entries.remove(new_name);
                }
            }
        }

        // Remove from old parent.
        if let Some(op) = inner.nodes.get_mut(&old_parent) {
            if let NodeKind::Directory(entries) = &mut op.kind {
                entries.remove(old_name);
            }
        }
        // Add to new parent.
        if let Some(np) = inner.nodes.get_mut(&new_parent) {
            if let NodeKind::Directory(entries) = &mut np.kind {
                entries.insert(new_name.to_string(), inum);
            }
        }
        // Re-parent the moved node.
        if let Some(child) = inner.nodes.get_mut(&inum) {
            child.parent = new_parent;
        }
        Ok(())
    }

    fn truncate(&self, inode: InodeNum, size: u64) -> FsResult<()> {
        let mut inner = self.inner.lock();
        let node = inner.nodes.get_mut(&inode).ok_or(FsError::NotFound)?;
        match &mut node.kind {
            NodeKind::File(data) => {
                data.resize(size as usize, 0);
                Ok(())
            }
            NodeKind::Directory(_) => Err(FsError::IsADirectory),
            NodeKind::Symlink(_) => Err(FsError::InvalidArgument),
        }
    }

    fn sync(&self) -> FsResult<()> {
        // Nothing to flush — we are the store.
        Ok(())
    }

    fn readlink(&self, inode: InodeNum) -> FsResult<String> {
        let inner = self.inner.lock();
        let node = inner.nodes.get(&inode).ok_or(FsError::NotFound)?;
        match &node.kind {
            NodeKind::Symlink(target) => Ok(target.clone()),
            _ => Err(FsError::InvalidArgument),
        }
    }

    fn symlink(&self, parent: InodeNum, name: &str, target: &str) -> FsResult<InodeNum> {
        validate_name(name)?;
        let mut inner = self.inner.lock();
        {
            let p = inner.nodes.get(&parent).ok_or(FsError::NotFound)?;
            match &p.kind {
                NodeKind::Directory(entries) => {
                    if entries.contains_key(name) {
                        return Err(FsError::AlreadyExists);
                    }
                }
                _ => return Err(FsError::NotADirectory),
            }
        }
        let inum = inner.next_inode;
        inner.next_inode = inner
            .next_inode
            .checked_add(1)
            .ok_or(FsError::NoSpace)?;
        inner.nodes.insert(
            inum,
            Node {
                parent,
                kind: NodeKind::Symlink(target.to_string()),
                mode: 0o777,
                uid: 0,
                gid: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
            },
        );
        if let Some(p) = inner.nodes.get_mut(&parent) {
            if let NodeKind::Directory(entries) = &mut p.kind {
                entries.insert(name.to_string(), inum);
            }
        }
        Ok(inum)
    }
}
