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
// The bridge socket server: the Unix socket the Runtime talks to.
// (Moved here from backend.rs in Stage 3 so the standalone backend can
// serve the same channel; the protocol is unchanged.)

use std::{
    io::{Read, Write},
    os::unix::{
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{diag, server::GenesisState};

/// The uid of the peer on a Unix socket, via `SO_PEERCRED`. (`std`'s
/// `peer_cred` is still unstable on this toolchain, so the check is done
/// with libc directly.)
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if ret == 0 {
        Some(cred.uid)
    } else {
        None
    }
}
/// One bridge request (one JSON object per line).
#[derive(Debug, Deserialize)]
struct BridgeRequest {
    /// Must equal [`BRIDGE_PROTOCOL_VERSION`].
    v: u32,
    op: String,
    /// The session to act on (required for `close`).
    session: Option<String>,
}

/// One bridge response (one JSON object per line).
#[derive(Debug, Serialize)]
struct BridgeResponse {
    v: u32,
    ok: bool,
    op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl BridgeResponse {
    fn ok(op: &str, session: Option<String>) -> Self {
        Self {
            v: BRIDGE_PROTOCOL_VERSION,
            ok: true,
            op: op.to_string(),
            session,
            error: None,
        }
    }

    fn err(op: &str, error: &str) -> Self {
        Self {
            v: BRIDGE_PROTOCOL_VERSION,
            ok: false,
            op: op.to_string(),
            session: None,
            error: Some(error.to_string()),
        }
    }
}

/// One connected bridge client, with its buffered input and output.
struct BridgeConnection {
    stream: UnixStream,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
}

/// The bridge socket server. Shared by the nested and standalone
/// backends (both serve the same Runtime channel).
pub(crate) struct BridgeServer {
    listener: UnixListener,
    connections: Vec<BridgeConnection>,
    /// The socket path, for removal at shutdown.
    pub(crate) path: PathBuf,
}

impl BridgeServer {
    pub(crate) fn bind(runtime_dir: &std::path::Path) -> std::io::Result<Self> {
        let path = runtime_dir.join(BRIDGE_SOCKET_NAME);
        // A stale socket from a crashed run would make bind fail; remove
        // it first (a live compositor would still hold the file open, so
        // this only ever removes a dead socket).
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        // Mode 0600: only the platform user may talk to the compositor.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            connections: Vec::new(),
            path,
        })
    }

    /// Accept pending connections (same-uid peers only) and serve one
    /// complete request line per connection per call. Never blocks.
    /// A duplicated fd for the listener, for calloop sources: the
    /// standalone backend polls this fd and calls [`Self::poll`] when it
    /// is readable. (A dup, not a borrow: the event loop owns the source
    /// while the server lives in the loop data.)
    pub(crate) fn poll_fd(&self) -> std::io::Result<std::os::unix::io::OwnedFd> {
        use std::os::unix::io::BorrowedFd;
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.listener.as_raw_fd()) };
        borrowed.try_clone_to_owned()
    }

    /// Accept pending connections (same-uid peers only) and serve one
    /// complete request line per connection per call. Never blocks.
    pub(crate) fn poll(&mut self, state: &mut GenesisState) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Trusted channel: the peer must be the same user.
                    // (Stage 4 adds pid-binding to the known Runtime pid.)
                    let allowed = peer_uid(&stream)
                        .map(|uid| uid == unsafe { libc::getuid() })
                        .unwrap_or(false);
                    if !allowed {
                        diag!("bridge: rejected a connection from another uid");
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if self.connections.len() >= 8 {
                        diag!("bridge: too many connections, dropping one");
                        continue;
                    }
                    self.connections.push(BridgeConnection {
                        stream,
                        incoming: Vec::new(),
                        outgoing: Vec::new(),
                    });
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        let mut index = 0;
        while index < self.connections.len() {
            let mut drop_connection = false;
            let mut response: Option<BridgeResponse> = None;
            {
                let connection = &mut self.connections[index];
                // Flush pending output first.
                if !connection.outgoing.is_empty() {
                    match connection.stream.write(&connection.outgoing) {
                        Ok(written) => {
                            connection.outgoing.drain(..written);
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => drop_connection = true,
                    }
                }
                // Read what is available.
                if !drop_connection {
                    let mut chunk = [0u8; 4096];
                    loop {
                        match connection.stream.read(&mut chunk) {
                            Ok(0) => {
                                drop_connection = true;
                                break;
                            }
                            Ok(read) => {
                                connection.incoming.extend_from_slice(&chunk[..read]);
                                if connection.incoming.len() > 4096 {
                                    drop_connection = true;
                                    break;
                                }
                            }
                            Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                break;
                            }
                            Err(ref err) if err.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(_) => {
                                drop_connection = true;
                                break;
                            }
                        }
                    }
                }
                // One complete line is one request.
                if !drop_connection {
                    if let Some(newline) = connection.incoming.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = connection.incoming.drain(..=newline).collect();
                        response = Some(handle_bridge_request(state, &line));
                    }
                }
            }
            if let Some(response) = response {
                let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
                bytes.push(b'\n');
                self.connections[index].outgoing.extend_from_slice(&bytes);
            }
            if drop_connection {
                self.connections.remove(index);
            } else {
                index += 1;
            }
        }
    }
}

/// Handle one bridge request line. `close` is the Shell asking the
/// compositor to close an application (ADR-0006 milestone 9); `ping` is
/// the liveness check.
fn handle_bridge_request(state: &mut GenesisState, line: &[u8]) -> BridgeResponse {
    let request: BridgeRequest = match serde_json::from_slice(line) {
        Ok(request) => request,
        Err(_) => return BridgeResponse::err("(unparsed)", "invalid JSON"),
    };
    if request.v != BRIDGE_PROTOCOL_VERSION {
        return BridgeResponse::err(&request.op, "unsupported protocol version");
    }
    match request.op.as_str() {
        "ping" => BridgeResponse::ok("ping", None),
        "close" => {
            let Some(session) = request.session else {
                return BridgeResponse::err("close", "missing session");
            };
            let Some(record) = state
                .registry
                .by_session(&SessionId(session.clone()))
                .map(|record| record.id)
            else {
                return BridgeResponse::err("close", "unknown session");
            };
            state.close_surface(record);
            state.needs_redraw = true;
            BridgeResponse::ok("close", Some(session))
        }
        _ => BridgeResponse::err(&request.op, "unknown op"),
    }
}
