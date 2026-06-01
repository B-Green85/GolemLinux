# `src/fs/` — Golem Linux Filesystem Layer

**Copyright (c) 2026 TrueSystems LLC. All rights reserved.**

Agent 5 of 6 — filesystem layer. Sibling agents own the other kernel
subsystems; this directory is self-contained and touches nothing outside
`src/fs/`.

---

## What is here

| File | Purpose |
| --- | --- |
| `mod.rs` | Public re-exports, the global `VFS` singleton, and the `init()` bring-up entry point. The only file the rest of the kernel needs to know about. |
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

## Kernel integration (Phase 2)

The filesystem now owns its own singleton and bring-up. The integration
agent (Agent 7) does **not** construct a `Vfs` or mount anything by hand —
it calls one function.

```rust
// In kernel_main, after the heap is up:
fs::init();
```

`fs::init()`:

1. Mounts a fresh `RamFs` at `/` — the root filesystem is in place before
   any userspace process runs.
2. Creates the baseline directory skeleton on that root: `/dev` (0755),
   `/proc` (0555), `/sys` (0555), `/tmp` (1777). These are conventional
   early mount points plus a writable scratch dir, so early userspace and
   later mounts (e.g. a real `devfs`) have somewhere to land. All four are
   pure ramfs operations; no other subsystem is touched.
3. Is **idempotent** — guarded by an atomic, a second call is a no-op.

It returns `()`. On a freshly-constructed `VFS` every step is structurally
infallible, so a failure means the mount table was corrupted before
bring-up; `init()` panics loudly in that case rather than booting onto a
broken root. `fs::is_initialized()` reports whether bring-up has completed.

The global instance is exposed as a public static, mirroring how the
sentinel subsystem exposes `SENTINEL`:

```rust
pub static VFS: Vfs = Vfs::new();   // in fs::mod
```

After `init()`, the rest of the kernel uses the filesystem through it:

```rust
let f = fs::VFS.open(
    "/init",
    OpenFlags { write: true, create: true, ..OpenFlags::default() },
)?;
f.write(b"#!/bin/golem-init\n")?;
```

### Ordering contract — heap first

**The heap MUST be initialized before `fs::init()` is called.** This
module allocates the instant `init()` runs — the ramfs inode table and
every directory below it live on the heap. Calling `fs::init()` before the
global allocator is registered will fault.

The agreed `kernel_main` ordering satisfies this:

| Order | Subsystem | Why |
| --- | --- | --- |
| 1 | `sentinel::init()` | governance gate, first by mandate |
| 2 | `memory::init(memory_map)` | **registers the heap allocator** |
| 3 | `fs::init()` | ← us; needs the heap from step 2 |
| 4 | `scheduler::init(...)` | needs the heap |
| 5 | `syscall::init()` | needs the scheduler |

### Crate-root requirements this module depends on (owned by Agent 7)

`src/fs/` is `no_std` + `alloc`-clean (no `std::` anywhere — audited), but
two **crate-level** declarations it cannot make itself must be present in
the crate root (`src/main.rs`):

1. `#![no_std]` — the operative no_std switch. (This module restates
   `#![cfg_attr(not(test), no_std)]` at its top to declare intent and stay
   consistent with the other subsystems; in a non-root module that line is
   a harmless `unused_attributes` warning and does nothing on its own.)
2. **`extern crate alloc;`** — required for the `use alloc::…` paths in
   `vfs.rs` and `ramfs.rs` to resolve. This must be at the crate root:
   declaring it in a sub-module (here, or in any sibling subsystem) does
   **not** populate the crate-wide extern prelude, so without it at the
   root every `alloc`-using subsystem fails to compile with `E0433`. This
   requirement is easy to miss — it is not in Agent 7's written field list.

### Adding mounts later

When the device-driver agent has a devfs:

```rust
fs::VFS.mount("/dev", DevFs::new())?;
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
- `alloc` — for `String`, `Vec`, `BTreeMap`, `Arc`. The kernel crate that
  contains this module **must declare `extern crate alloc;` at its root**
  (`src/main.rs`) — not in a sub-module; a sub-module declaration does not
  reach sibling modules (see "Crate-root requirements" above) — and must
  provide a global allocator that is live before `fs::init()` runs (the
  memory agent's responsibility).
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
