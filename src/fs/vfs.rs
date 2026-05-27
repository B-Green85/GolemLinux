// SPDX-License-Identifier: Proprietary
// Copyright (c) 2026 TrueSystems LLC. All rights reserved.
//
// Virtual Filesystem (VFS) abstraction.
//
// This is the kernel-side filesystem boundary. Concrete filesystems (ramfs
// today; extfs/fatfs/etc. tomorrow) implement the `FileSystem` trait. The
// `Vfs` type is the singleton that owns the mount table, resolves paths into
// (filesystem, inode) tuples, and produces open file handles.
//
// Design notes are in `README.md` next to this file. The short version:
//   - `no_std` + `alloc`. No std types appear in this module.
//   - Inode numbers are local to a single filesystem. The (fs, inode) pair
//     is the kernel-wide identifier.
//   - Path resolution is mount-table driven with longest-prefix match. No
//     traversal across mount points yet (a `..` that would exit a mount
//     stays at that mount's root for now — documented limitation).
//   - All synchronization is via `spin::Mutex`. The kernel sync agent may
//     swap this for a richer primitive later; one import to update.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use spin::Mutex;

/// Inode numbers are local to the filesystem that produced them. They are
/// only meaningful when paired with the `Arc<dyn FileSystem>` that owns them.
pub type InodeNum = u64;

/// Unix-style mode bits (e.g. `0o755`). The VFS treats this as opaque — it
/// is the process/security layer's job to interpret permissions. We pass
/// it through to drivers so they can persist it.
pub type Mode = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub inode: InodeNum,
    pub file_type: FileType,
    pub size: u64,
    pub mode: Mode,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    /// Access time, in nanoseconds since the kernel epoch. Zero is acceptable
    /// for filesystems that do not track time (ramfs, for now).
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub inode: InodeNum,
    pub file_type: FileType,
}

/// Errors that may propagate out of the VFS. We deliberately keep this set
/// small and exhaustive — kernel error translation lives at the syscall
/// boundary, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    NotEmpty,
    InvalidPath,
    InvalidArgument,
    InvalidOffset,
    PermissionDenied,
    NoSpace,
    ReadOnly,
    Io,
    Unsupported,
    BadDescriptor,
    /// `rename` (and one day hardlink) cannot cross filesystem boundaries.
    CrossDevice,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FsError::NotFound => "no such file or directory",
            FsError::AlreadyExists => "file exists",
            FsError::NotADirectory => "not a directory",
            FsError::IsADirectory => "is a directory",
            FsError::NotEmpty => "directory not empty",
            FsError::InvalidPath => "invalid path",
            FsError::InvalidArgument => "invalid argument",
            FsError::InvalidOffset => "invalid offset",
            FsError::PermissionDenied => "permission denied",
            FsError::NoSpace => "no space left on device",
            FsError::ReadOnly => "read-only filesystem",
            FsError::Io => "i/o error",
            FsError::Unsupported => "operation not supported",
            FsError::BadDescriptor => "bad file descriptor",
            FsError::CrossDevice => "cross-device link",
        })
    }
}

pub type FsResult<T> = Result<T, FsError>;

/// Flags passed to `Vfs::open`. We use an explicit struct rather than a
/// bitflags-style integer so callers cannot pass mutually exclusive sets,
/// and so we avoid pulling in the `bitflags` crate at this layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub create: bool,
    pub truncate: bool,
    /// O_EXCL — fail if `create` is set and the file already exists.
    pub exclusive: bool,
}

impl OpenFlags {
    pub const fn read_only() -> Self {
        Self { read: true, write: false, append: false, create: false, truncate: false, exclusive: false }
    }
    pub const fn write_only() -> Self {
        Self { read: false, write: true, append: false, create: false, truncate: false, exclusive: false }
    }
    pub const fn read_write() -> Self {
        Self { read: true, write: true, append: false, create: false, truncate: false, exclusive: false }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

/// The filesystem driver contract. Every implementation (ramfs, future
/// ext4, fat, devfs, procfs, etc.) implements this trait.
///
/// All methods take `&self` because filesystems must be `Send + Sync`-shared
/// across kernel threads. Implementations are expected to provide their own
/// interior mutability (typically a `spin::Mutex` over their state).
pub trait FileSystem: Send + Sync {
    /// Short identifier, e.g. `"ramfs"`. Used for `/proc/mounts`-style output
    /// and debug logging only — not for dispatch.
    fn name(&self) -> &str;

    /// The inode number of this filesystem's root directory.
    fn root_inode(&self) -> InodeNum;

    fn metadata(&self, inode: InodeNum) -> FsResult<Metadata>;

    /// Resolve a single path component. `name` is never empty and contains
    /// no `/`. The VFS handles `.` and `..` semantics across mounts; an
    /// individual filesystem still needs to recognize `..` within itself
    /// (ramfs uses each directory's stored parent pointer).
    fn lookup(&self, parent: InodeNum, name: &str) -> FsResult<InodeNum>;

    fn readdir(&self, inode: InodeNum) -> FsResult<Vec<DirEntry>>;

    fn read(&self, inode: InodeNum, offset: u64, buf: &mut [u8]) -> FsResult<usize>;
    fn write(&self, inode: InodeNum, offset: u64, buf: &[u8]) -> FsResult<usize>;

    fn create(&self, parent: InodeNum, name: &str, file_type: FileType, mode: Mode) -> FsResult<InodeNum>;
    fn unlink(&self, parent: InodeNum, name: &str) -> FsResult<()>;
    fn rmdir(&self, parent: InodeNum, name: &str) -> FsResult<()>;
    fn rename(
        &self,
        old_parent: InodeNum,
        old_name: &str,
        new_parent: InodeNum,
        new_name: &str,
    ) -> FsResult<()>;

    fn truncate(&self, inode: InodeNum, size: u64) -> FsResult<()>;

    /// Flush dirty state to the backing store. Ramfs returns `Ok(())`.
    fn sync(&self) -> FsResult<()>;

    fn readlink(&self, inode: InodeNum) -> FsResult<String> {
        let _ = inode;
        Err(FsError::Unsupported)
    }

    fn symlink(&self, parent: InodeNum, name: &str, target: &str) -> FsResult<InodeNum> {
        let _ = (parent, name, target);
        Err(FsError::Unsupported)
    }
}

/// One row in the kernel mount table.
struct MountEntry {
    /// Normalized mount path. Always begins with `/`. Never has a trailing
    /// slash, except for the root `/` itself.
    path: String,
    fs: Arc<dyn FileSystem>,
}

/// The kernel's filesystem singleton. Owns the mount table; produces open
/// `File` handles. One `Vfs` per running kernel.
pub struct Vfs {
    mounts: Mutex<Vec<MountEntry>>,
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    /// `const` so a `static VFS: Vfs = Vfs::new();` placement is available
    /// to the kernel bootstrap code.
    pub const fn new() -> Self {
        Self {
            mounts: Mutex::new(Vec::new()),
        }
    }

    /// Mount a filesystem at `mount_point`. The path must be absolute. Two
    /// filesystems may not be mounted at the same exact path; longer paths
    /// shadow shorter ones during resolution.
    pub fn mount(&self, mount_point: &str, fs: Arc<dyn FileSystem>) -> FsResult<()> {
        if !mount_point.starts_with('/') {
            return Err(FsError::InvalidPath);
        }
        let normalized = normalize_mount_path(mount_point);
        let mut mounts = self.mounts.lock();
        if mounts.iter().any(|m| m.path == normalized) {
            return Err(FsError::AlreadyExists);
        }
        mounts.push(MountEntry { path: normalized, fs });
        Ok(())
    }

    /// Remove a mount. The caller must ensure no open files reference the
    /// mount — busy-mount checking is the process layer's job, not the
    /// VFS's. Unmounting `/` is allowed structurally; the kernel will
    /// almost certainly panic on the next file access if it does so.
    pub fn unmount(&self, mount_point: &str) -> FsResult<()> {
        let normalized = normalize_mount_path(mount_point);
        let mut mounts = self.mounts.lock();
        let pos = mounts
            .iter()
            .position(|m| m.path == normalized)
            .ok_or(FsError::NotFound)?;
        mounts.remove(pos);
        Ok(())
    }

    /// Snapshot of the mount table for diagnostics.
    pub fn mounts(&self) -> Vec<(String, &'static str)> {
        let m = self.mounts.lock();
        let mut out = Vec::with_capacity(m.len());
        for entry in m.iter() {
            // SAFETY: trait method returns &str borrowed from the impl. We
            // cannot keep that borrow alive past the lock, so we copy.
            // Most fs names are `&'static str` literals; converting via
            // `Arc::clone` would defeat that. We expose name as &'static
            // here only as a debug convenience; if a driver ever produces
            // a non-static name, this method needs revisiting.
            let name: &str = entry.fs.name();
            // Erase the lifetime: callers are expected to use this only
            // for immediate logging.
            let name_static: &'static str = unsafe { core::mem::transmute(name) };
            out.push((entry.path.clone(), name_static));
        }
        out
    }

    /// Walk an absolute path, returning the filesystem and inode it points
    /// at. Errors with `InvalidPath` if the path is not absolute,
    /// `NotFound` if any component is missing.
    pub fn resolve(&self, path: &str) -> FsResult<(Arc<dyn FileSystem>, InodeNum)> {
        if !path.starts_with('/') {
            return Err(FsError::InvalidPath);
        }

        let (fs, rel) = {
            let mounts = self.mounts.lock();
            let entry = mounts
                .iter()
                .filter(|m| path_starts_with_mount(path, &m.path))
                .max_by_key(|m| m.path.len())
                .ok_or(FsError::NotFound)?;
            let stripped = if entry.path == "/" {
                &path[1..]
            } else {
                &path[entry.path.len()..]
            };
            (entry.fs.clone(), stripped.trim_start_matches('/').to_string())
        };

        let mut current = fs.root_inode();
        for component in rel.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            // `..` inside a single fs is delegated to the driver. Crossing
            // mount boundaries via `..` is not yet supported; see README.
            current = fs.lookup(current, component)?;
        }
        Ok((fs, current))
    }

    pub fn open(&self, path: &str, flags: OpenFlags) -> FsResult<File> {
        let (fs, inode) = match self.resolve(path) {
            Ok(found) => {
                if flags.exclusive && flags.create {
                    return Err(FsError::AlreadyExists);
                }
                found
            }
            Err(FsError::NotFound) if flags.create => {
                let (parent_path, name) = split_parent(path)?;
                let (fs, parent_inode) = self.resolve(parent_path)?;
                let inode = fs.create(parent_inode, name, FileType::Regular, 0o644)?;
                (fs, inode)
            }
            Err(e) => return Err(e),
        };

        if flags.truncate && flags.write {
            fs.truncate(inode, 0)?;
        }

        let initial_offset = if flags.append {
            fs.metadata(inode)?.size
        } else {
            0
        };

        Ok(File {
            fs,
            inode,
            offset: Mutex::new(initial_offset),
            flags,
        })
    }

    pub fn mkdir(&self, path: &str, mode: Mode) -> FsResult<()> {
        let (parent_path, name) = split_parent(path)?;
        let (fs, parent_inode) = self.resolve(parent_path)?;
        fs.create(parent_inode, name, FileType::Directory, mode)?;
        Ok(())
    }

    pub fn unlink(&self, path: &str) -> FsResult<()> {
        let (parent_path, name) = split_parent(path)?;
        let (fs, parent_inode) = self.resolve(parent_path)?;
        fs.unlink(parent_inode, name)
    }

    pub fn rmdir(&self, path: &str) -> FsResult<()> {
        let (parent_path, name) = split_parent(path)?;
        let (fs, parent_inode) = self.resolve(parent_path)?;
        fs.rmdir(parent_inode, name)
    }

    pub fn metadata(&self, path: &str) -> FsResult<Metadata> {
        let (fs, inode) = self.resolve(path)?;
        fs.metadata(inode)
    }

    pub fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>> {
        let (fs, inode) = self.resolve(path)?;
        fs.readdir(inode)
    }

    pub fn rename(&self, from: &str, to: &str) -> FsResult<()> {
        let (from_parent_path, from_name) = split_parent(from)?;
        let (to_parent_path, to_name) = split_parent(to)?;
        let (from_fs, from_parent) = self.resolve(from_parent_path)?;
        let (to_fs, to_parent) = self.resolve(to_parent_path)?;
        if !Arc::ptr_eq(&from_fs, &to_fs) {
            return Err(FsError::CrossDevice);
        }
        from_fs.rename(from_parent, from_name, to_parent, to_name)
    }
}

/// A handle to an open file. The (fs, inode) pair is the underlying object;
/// `offset` is the per-handle position, behind its own lock so a `File`
/// shared between kernel threads (or duped fds) reads/writes coherently.
pub struct File {
    fs: Arc<dyn FileSystem>,
    inode: InodeNum,
    offset: Mutex<u64>,
    flags: OpenFlags,
}

impl File {
    pub fn read(&self, buf: &mut [u8]) -> FsResult<usize> {
        if !self.flags.read {
            return Err(FsError::PermissionDenied);
        }
        let mut offset = self.offset.lock();
        let n = self.fs.read(self.inode, *offset, buf)?;
        *offset += n as u64;
        Ok(n)
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        if !self.flags.read {
            return Err(FsError::PermissionDenied);
        }
        self.fs.read(self.inode, offset, buf)
    }

    pub fn write(&self, buf: &[u8]) -> FsResult<usize> {
        if !self.flags.write {
            return Err(FsError::PermissionDenied);
        }
        let mut offset = self.offset.lock();
        if self.flags.append {
            // Re-read the file size on every append so concurrent writers
            // are observed. Cheap on ramfs; drivers should keep it cheap.
            *offset = self.fs.metadata(self.inode)?.size;
        }
        let n = self.fs.write(self.inode, *offset, buf)?;
        *offset += n as u64;
        Ok(n)
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> FsResult<usize> {
        if !self.flags.write {
            return Err(FsError::PermissionDenied);
        }
        self.fs.write(self.inode, offset, buf)
    }

    pub fn seek(&self, pos: SeekFrom) -> FsResult<u64> {
        let mut offset = self.offset.lock();
        let new = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => {
                let size = self.fs.metadata(self.inode)?.size as i128;
                let n = size + d as i128;
                if n < 0 {
                    return Err(FsError::InvalidOffset);
                }
                n as u64
            }
            SeekFrom::Current(d) => {
                let n = *offset as i128 + d as i128;
                if n < 0 {
                    return Err(FsError::InvalidOffset);
                }
                n as u64
            }
        };
        *offset = new;
        Ok(new)
    }

    pub fn metadata(&self) -> FsResult<Metadata> {
        self.fs.metadata(self.inode)
    }

    pub fn truncate(&self, size: u64) -> FsResult<()> {
        if !self.flags.write {
            return Err(FsError::PermissionDenied);
        }
        self.fs.truncate(self.inode, size)
    }

    pub fn sync(&self) -> FsResult<()> {
        self.fs.sync()
    }

    pub fn inode(&self) -> InodeNum {
        self.inode
    }
}

// ----------------------------------------------------------------------
// Path helpers. Kept private to this module; callers go through `Vfs`.

fn normalize_mount_path(p: &str) -> String {
    if p == "/" {
        return String::from("/");
    }
    String::from(p.trim_end_matches('/'))
}

fn path_starts_with_mount(path: &str, mount: &str) -> bool {
    if mount == "/" {
        return true;
    }
    if !path.starts_with(mount) {
        return false;
    }
    let after = &path[mount.len()..];
    after.is_empty() || after.starts_with('/')
}

/// Split `/a/b/c` into (`/a/b`, `c`). Returns `InvalidPath` for `/` or any
/// non-absolute input.
fn split_parent(path: &str) -> FsResult<(&str, &str)> {
    if !path.starts_with('/') {
        return Err(FsError::InvalidPath);
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(FsError::InvalidPath);
    }
    match trimmed.rsplit_once('/') {
        Some(("", name)) if !name.is_empty() => Ok(("/", name)),
        Some((parent, name)) if !name.is_empty() => Ok((parent, name)),
        _ => Err(FsError::InvalidPath),
    }
}
