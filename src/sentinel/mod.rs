//! Sentinel subsystem.
//!
//! Sentinel is Golem's host-side trust authority. Agents running on the host
//! register with it across a kernel-mediated channel before they are granted a
//! permission tier. This module groups the kernel-side pieces of that system.
//!
//! Phase 4 / Agent 1 deliverable: [`ipc`] — the virtio-serial IPC channel that
//! carries the registration handshake between the Golem guest kernel and the
//! host Sentinel daemon. See that module's docs for the wire protocol and the
//! VirtIO driver bring-up.

pub mod ipc;

// Convenience re-exports of the IPC entry points so callers can write
// `sentinel::init()` / `sentinel::handle_one()` without reaching into `ipc`.
pub use ipc::{
    build_response, handle_one, init, parse_register_request, read_request, serialize_response,
    write_response_bytes, IpcError, PermissionTier, RegisterRequest, RegisterResponse,
};
