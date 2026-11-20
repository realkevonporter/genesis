// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime bridge — the versioned surface ↔ Runtime-session association.
//!
//! Stage 2's part of the Runtime↔compositor interface (see
//! COMPOSITOR_PLAN.md, Stage 2 milestone 6: "the compositor associates that
//! surface with a Runtime session", and milestone 10: "the Runtime records
//! the stopped state").
//!
//! What exists in Stage 2:
//!
//! * [`SessionId`] — the opaque id of a Runtime session. Until the Runtime
//!   exists and passes real session ids over the bridge socket (Stage 4),
//!   the id is derived deterministically from the client pid:
//!   `runtime:<pid>`. That derivation is provisional and documented here,
//!   not a Runtime API: no Runtime behaviour is invented.
//! * [`SessionAssociation`] — the record for one surface: protocol version,
//!   session id, surface id, app id, client pid. [`associate`] emits the
//!   `SESSION ASSOCIATED` marker; [`disassociate`] emits `SESSION STOPPED`.
//!   Those markers are the record the (future) Runtime consumes: the
//!   compositor writes them, the Runtime reads them. The Runtime itself
//!   recording the stopped state is a Stage 4+ consumer of this record.
//! * The bridge socket (see `backend.rs`): a Unix socket speaking
//!   newline-delimited JSON, protocol version [`BRIDGE_PROTOCOL_VERSION`],
//!   with the Stage 2 op set (`ping`, `close`). Stage 4 extends the same
//!   socket with `adopt` / `move` / `resize` / `release` — the op set is
//!   additive within the version, and the socket path and auth story
//!   (`SO_PEERCRED`, same-uid peer, mode 0600) are the plan's.
//!
//! What does NOT exist yet (documented next steps, not silent gaps):
//!
//! * No Runtime process, so nothing consumes the markers yet and no
//!   session id arrives from the Runtime.
//! * No pid-binding to a known Runtime pid at launch (Stage 4: init passes
//!   the Runtime pid to Genesis; the socket then admits only that pid).
//! * No `adopt` / `move` / `resize` / `release` ops (Stage 4).

use crate::marker;

/// The bridge protocol version. Every bridge message carries `"v": 1`.
/// Ops are additive within a version; a version bump is a deliberate,
/// documented change, not drift.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// The Unix socket path, relative to `$XDG_RUNTIME_DIR`.
/// Mirrors the plan's `/run/sevyn-compositor.sock` (on SevynOS
/// `$XDG_RUNTIME_DIR` is `/run/user/<uid>`; the plan's `/run/...` path is
/// the same socket seen from the system side).
pub const BRIDGE_SOCKET_NAME: &str = "sevyn-compositor.sock";

/// The opaque id of a Runtime session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    /// Provisional derivation (see the module docs): until the Runtime
    /// passes real session ids, a session is identified by its client pid.
    pub fn for_client_pid(pid: u32) -> Self {
        SessionId(format!("runtime:{pid}"))
    }

    /// Fallback when the client pid is unknown (should not happen for a
    /// local Wayland client; kept so association never silently fails).
    pub fn unknown(surface_id: u64) -> Self {
        SessionId(format!("runtime:unknown-{surface_id}"))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One surface ↔ session association, at bridge protocol version 1.
/// (The version itself lives in [`BRIDGE_PROTOCOL_VERSION`]; the record
/// is always written at the current version.)
#[derive(Debug, Clone)]
pub struct SessionAssociation {
    pub session: SessionId,
    /// The compositor's surface id (see `surface_registry`).
    pub surface_id: u64,
    /// The client's `xdg_toplevel` app id, when it has set one.
    pub app_id: String,
    /// The client's pid from the Wayland connection credentials.
    pub pid: Option<u32>,
}

/// Record a new association: emit the `SESSION ASSOCIATED` marker.
pub fn associate(association: &SessionAssociation) {
    marker!(
        "SESSION ASSOCIATED session={} surface={} app_id={} pid={}",
        association.session,
        association.surface_id,
        if association.app_id.is_empty() {
            "(none)"
        } else {
            &association.app_id
        },
        association
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "(unknown)".to_string())
    );
}

/// Record the end of an association: emit the `SESSION STOPPED` marker.
///
/// `reason` is `closed` when the Shell asked for the close through the
/// bridge, `disconnected` when the client went away on its own. The
/// Runtime consumes this marker to record the stopped state (ADR-0006
/// milestone 10); until the Runtime exists, the marker is the record.
pub fn disassociate(session: &SessionId, surface_id: u64, reason: &str) {
    marker!("SESSION STOPPED session={session} surface={surface_id} reason={reason}");
}
