# `src/fs/` — Golem Linux Filesystem Layer

**Copyright (c) 2026 TrueSystems LLC. All rights reserved.**

Agent 5 of 6 — filesystem layer. Sibling agents own the other kernel
subsystems; this directory is self-contained and touches nothing outside
`src/fs/`.

---

## What is here

| File | Purpose |
| --- | --- |
| `mod.rs` | Public re-exports. The only file the rest of the kernel needs to know about. |
| `vfs.rs` | The Virtual Filesystem abstraction: `FileSystem` trait, `Vfs` mount table, `File` handles, error types. |
| `ramfs.rs` | A complete in-memory `FileSystem` implementation. Mountable at `/` to let the kernel boot before any block driver exists. |
| `README.md` | This document. |

---

## Design goals (and why)

1. **Boot first, optimize later.** Golem needs *a* filesystem to mount at
   `/` before the disk stack comes online. RamFs is that filesystem.
   It does not need to be fast or persistent — it needs to be correct,
   small, and obviously right.

2. **One trait per driver, no exceptions.** Every filesystem — current
   and future — implements `vfs::FileSystem`. The trait is the contract;
   adding ext4, FAT, devfs, procfs, etc. is a matter of writing one more
   `impl FileSystem for ...` block. The VFS itself never grows a `match`
   on driver type.

3. **`no_std` + `alloc`.** This is kernel code. We use `core` and `alloc`.
   No `std` types appear anywhere in this module. Heap allocation is
   assumed available (provided by the memory-management agent).

4. **Inode numbers are filesystem-local.** A `u64` inode is meaningful
   only when paired with an `Arc<dyn FileSystem>`. The VFS hands those
   pairs out via `Vfs::resolve(path)`; the kernel's per-process fd table
   should store `Arc<File>` (which carries both).

5. **Synchronization is `spin::Mutex` for now.** Kernel-suitable, lock-
   free of the standard library, well-tested. One import to change if
   the sync agent later provides a richer primitive.

6. **Errors are exhaustive and small.** `FsError` is a closed enum.
   Syscall translation is the job of the syscall layer, not the VFS.

---

## The VFS abstraction (`vfs.rs`)

### `trait FileSystem`

Every driver implements this. All methods take `&self`, so drivers must
provide their own interior mutability (RamFs uses a single `Mutex` over
its inode table; an ext4 driver would likely use finer-grained locking).

The trait is deliberately POSIX-shaped — `lookup`, `readdir`, `create`,
`unlink`, `rmdir`, `rename`, `read`, `write`, `truncate`, `sync`,
`metadata`, `readlink`, `symlink`. There are no surprises here for anyone
who has read a Linux fs driver before, which is the point. Drivers
should feel familiar to port.

Two optional methods (`readlink`, `symlink`) have default `Unsupported`
implementations so a driver that does not care about symlinks does not
need to write boilerplate.

### `struct Vfs` — the mount table

The kernel constructs a single `Vfs`, mounts ramfs at `/`, and treats it
as the global filesystem root for all path operations.

`Vfs::new()` is `const`, so a `static VFS: Vfs = Vfs::new();` placement
in the kernel's bootstrap is supported.

**Path resolution rules:**
- Paths must be absolute (begin with `/`). Relative paths are the
  process layer's responsibility; the VFS does not have a CWD.
- The mount table is matched by **longest prefix**. So if `/` and
  `/dev` are both mounted, `/dev/null` resolves through the `/dev`
  filesystem, not the root one.
- Inside a single filesystem, `.` is skipped and `..` is delegated to
  the driver (which is expected to track each directory's parent — see
  ramfs's `Node.parent`).
- Crossing mount boundaries via `..` is **not yet supported**.
  `/dev/..` will resolve to the root of the devfs mount, not to `/`.
  This is a known limitation. Fixing it requires the VFS to remember
  the mount-point's parent inode in the host filesystem, which is a
  small follow-up and is intentionally scoped out of the bootstrap.

### `struct File` — open file handles

A `File` wraps `(Arc<dyn FileSystem>, InodeNum)` and adds a per-handle
seek offset. The offset is behind its own `Mutex` so kernel threads
sharing the same handle (or duped fds) get coherent reads/writes.

The kernel's fd table should hold `Arc<File>`, not `File` directly, so
`dup(2)`-style sharing is a pointer copy.

### Error handling

`FsError` is one byte's worth of discriminant. `FsResult<T>` is its
shorthand. The set is intentionally small:

| Error | Maps roughly to |
| --- | --- |
| `NotFound` | `ENOENT` |
| `AlreadyExists` | `EEXIST` |
| `NotADirectory` | `ENOTDIR` |
| `IsADirectory` | `EISDIR` |
| `NotEmpty` | `ENOTEMPTY` |
| `InvalidPath` / `InvalidArgument` / `InvalidOffset` | `EINVAL` |
| `PermissionDenied` | `EACCES` / `EPERM` |
| `NoSpace` | `ENOSPC` |
| `ReadOnly` | `EROFS` |
| `Io` | `EIO` |
| `Unsupported` | `ENOTSUP` |
| `BadDescriptor` | `EBADF` |
| `CrossDevice` | `EXDEV` |

The mapping to errno values is the syscall layer's job — the VFS keeps
the semantic names.

---

## RamFs (`ramfs.rs`)

RamFs is the bootstrap filesystem. It exists so the kernel can mount
*something* at `/` immediately.

### Storage layout

- `BTreeMap<InodeNum, Node>` — the inode table. Chosen over a hash map
  to avoid `hashbrown`; ramfs is not a hot path, and `BTreeMap` gives
  deterministic readdir ordering (useful for debugging early boot).
- Each `Node` is one of `File(Vec<u8>)`, `Directory(BTreeMap<name, inode>)`,
  or `Symlink(String)`.
- Directories store their own parent inode, so `..` is `O(1)`.

### Concurrency

One `spin::Mutex` around the whole inode table. Coarse, but simple and
correct. If profiling shows this lock is contended, the table can be
sharded by inode, or moved behind an `RwLock`. Not worth doing now.

### What ramfs deliberately does not do

- No persistence. The whole tree dies at reboot.
- No timestamps. `atime`/`mtime`/`ctime` are zero. They will start
  ticking when the kernel time agent exposes a monotonic clock.
- No uid/gid tracking. Everything is owned by root (0/0). The process
  agent owns identity; ramfs has nothing to enforce against.
- No permission checks. Same reason — the security context lives above
  us. The VFS passes mode bits through; ramfs stores them.
- No device, fifo, or socket nodes. `create` rejects those file types
  with `Unsupported`. They will be wired through when the device-driver
  agent's `devfs` lands.

### What it *does* do

- Files: read, write, truncate, sparse-write zero-extension.
- Directories: create, lookup, readdir (with `.` and `..` synthetic
  entries), rmdir-when-empty.
- Symlinks: create, readlink. Path resolution does not follow symlinks
  yet — that is a VFS-level decision pending design alignment with the
  security/Sentinel agent (symlink-following is a classic privilege-
  escalation vector).
- Rename: same-fs only, handles directory-overwrite-empty-directory
  and file-overwrite-file semantics, rejects cycles when moving a
  directory into its own descendant.

---

## How the kernel uses this

Sketch of expected usage (the kernel-bootstrap agent will own the real
call site):

```rust
use fs::{RamFs, Vfs, OpenFlags};

static VFS: Vfs = Vfs::new();

fn early_boot() {
    let root = RamFs::new();
    VFS.mount("/", root).unwrap();

    VFS.mkdir("/dev", 0o755).unwrap();
    VFS.mkdir("/proc", 0o555).unwrap();
    VFS.mkdir("/sentinel", 0o700).unwrap();

    let init = VFS.open(
        "/init",
        OpenFlags { write: true, create: true, ..OpenFlags::default() },
    ).unwrap();
    init.write(b"#!/bin/golem-init\n").unwrap();
}
```

Later, when the device-driver agent has a devfs:

```rust
VFS.mount("/dev", DevFs::new()).unwrap();
```

The `/dev` mount shadows the corresponding directory in ramfs (longest-
prefix wins). No changes to ramfs or the VFS are required to add new
drivers — that is the whole point.

---

## Adding a new filesystem driver

1. Add a new module in this directory, e.g. `pub mod extfs;` in
   `mod.rs`.
2. Define your driver type and implement `vfs::FileSystem` for it.
   The contract is small; about 12 methods, two of which have default
   implementations.
3. Return your type as `Arc<Self>` from a constructor, the same way
   `RamFs::new` does, so callers can mount it without ceremony.
4. Re-export your driver's main type from `mod.rs` if it should be
   part of the public surface.

That's it. The VFS does not change. The kernel does not change. The
process layer does not change.

---

## Dependencies

This module depends on:

- `core` — implicit, always available.
- `alloc` — for `String`, `Vec`, `BTreeMap`, `Arc`, `Box`. The kernel
  crate that contains this module must declare `extern crate alloc;`
  at its root and provide a global allocator (the memory agent's
  responsibility).
- `spin` — for `Mutex`. The de-facto kernel synchronization crate in
  the Rust ecosystem. If the sync agent later provides an in-tree
  primitive, swap the `use spin::Mutex;` lines in `vfs.rs` and
  `ramfs.rs` — one import per file.

Nothing else. No `bitflags`, no `hashbrown`, no `lazy_static`. Keeping
the dependency surface minimal at the boot path is deliberate.

---

## Testing

The crate-level test harness is owned by the build/test agent.
This module is structured to be testable both in a `no_std` kernel
context and against `cargo test` on a host:

- `RamFs` is allocation-only — it has no `unsafe`, no inline asm, no
  hardware access. It compiles cleanly under host `std` as long as
  `alloc` is in scope.
- `Vfs` is the same; it operates purely over the `FileSystem` trait.

A future PR is expected to add unit tests under `#[cfg(test)]` once the
crate's test infrastructure is decided. The interfaces here are stable
enough to test against now.

---

## Known limitations and follow-up work

- `..` does not cross mount boundaries (see VFS section).
- No symlink-following during path resolution (deliberate; coordinate
  with the security agent before enabling).
- No device/fifo/socket nodes in ramfs (waits on devfs).
- No timestamps (waits on kernel clock).
- No permission/ACL enforcement at this layer (correct; lives above
  the VFS).
- No buffer cache. Each `read`/`write` syscall takes the driver's
  lock. Acceptable for ramfs; will need addressing for block-backed
  filesystems — but a buffer cache belongs in its own module, not
  inside the VFS.

---

*Golem Linux is developed under the CDMAE methodology.*
