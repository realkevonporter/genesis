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
//!   with the Stage 2 op set (`ping`, `close`) plus the Stage 4 adoption
//!   ops (`adopt`, `move`, `resize`, `release` — see `adoption.rs`).
//!   The op set is additive within the version, and the socket path and
//!   auth story (`SO_PEERCRED`, same-uid peer, mode 0600) are the plan's;
//!   Stage 4 adds pid-binding to the known Runtime pid (see
//!   `--runtime-pid` and `BridgeServer::bind`). Screen capture adds the
//!   `capture` op (see `capture.rs` for the readback and the S3 rule):
//!   `{"v": 1, "op": "capture"}` renders the scene once into an
//!   offscreen target and writes `/data/screen/latest.png` (overwritten
//!   on every capture — S4 keeps exactly one frame), replying
//!   `{"ok": true, "op": "capture", "width": <w>, "height": <h>,
//!   "path": "<path>"}`. An optional absolute `"path"` overrides the
//!   output (for tests — the CI user cannot write `/data`); a
//!   relative or empty path is `"malformed request"`. The reply is
//!   deferred one loop iteration: the handler only records the
//!   request, and the backend resolves it after the next render, so
//!   the reply arrives on the same connection shortly after the
//!   request. When a sensitive surface is visible the op refuses
//!   outright (`{"ok": false, "error": "refused: sensitive surface"}`)
//!   and writes nothing, leaving the last good frame untouched.
//!
//! What does NOT exist yet (documented next steps, not silent gaps):
//!
//! * No Runtime process, so nothing consumes the markers yet and no
//!   session id arrives from the Runtime — except through the bridge
//!   ops themselves, which is how the adoption check drives it.
//! * No separate `register` op: a Runtime (re)connection is itself the
//!   re-registration (see `BridgeServer::poll`).

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
        io::{AsRawFd, BorrowedFd, OwnedFd},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{diag, server::GenesisState};

/// The full `SO_PEERCRED` credentials of the peer on a Unix socket.
fn peer_cred(stream: &UnixStream) -> Option<libc::ucred> {
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
        Some(cred)
    } else {
        None
    }
}

/// A content rect in Browser-window-local coordinates, as sent by the
/// Runtime in `adopt` / `move` / `resize`.
#[derive(Debug, Clone, Copy, Deserialize)]
struct RectJson {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// One bridge request (one JSON object per line).
#[derive(Debug, Deserialize)]
struct BridgeRequest {
    /// Must equal [`BRIDGE_PROTOCOL_VERSION`].
    v: u32,
    op: String,
    /// The session to act on (required for `close`, `adopt`, `move`,
    /// `resize`, `release`).
    session: Option<String>,
    /// The Chromium pid, for `adopt` only.
    chromium_pid: Option<u32>,
    /// The content rect, for `adopt` / `move` / `resize` only.
    rect: Option<RectJson>,
    /// The output path, for `capture` only (absolute; the default is
    /// [`crate::capture::DEFAULT_CAPTURE_PATH`]).
    path: Option<String>,
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
    /// The captured frame width, for `capture` success only.
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    /// The captured frame height, for `capture` success only.
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    /// The PNG that was written, for `capture` success only.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

impl BridgeResponse {
    fn ok(op: &str, session: Option<String>) -> Self {
        Self {
            v: BRIDGE_PROTOCOL_VERSION,
            ok: true,
            op: op.to_string(),
            session,
            error: None,
            width: None,
            height: None,
            path: None,
        }
    }

    fn err(op: &str, error: &str) -> Self {
        Self {
            v: BRIDGE_PROTOCOL_VERSION,
            ok: false,
            op: op.to_string(),
            session: None,
            error: Some(error.to_string()),
            width: None,
            height: None,
            path: None,
        }
    }

    fn capture_ok(width: u32, height: u32, path: &str) -> Self {
        Self {
            v: BRIDGE_PROTOCOL_VERSION,
            ok: true,
            op: "capture".to_string(),
            session: None,
            error: None,
            width: Some(width),
            height: Some(height),
            path: Some(path.to_string()),
        }
    }
}

/// Serialise a capture success reply. The backends build the reply
/// for a capture they resolved (see `capture.rs`); the JSON goes to
/// `GenesisState::capture_reply` and `BridgeServer::poll` drains it.
pub(crate) fn capture_ok_json(width: u32, height: u32, path: &str) -> String {
    serde_json::to_string(&BridgeResponse::capture_ok(width, height, path))
        .unwrap_or_else(|_| "{\"v\":1,\"ok\":true,\"op\":\"capture\"}".to_string())
}

/// Serialise a capture failure reply (readback / write error, or a
/// sensitive surface found at capture time).
pub(crate) fn capture_err_json(error: &str) -> String {
    serde_json::to_string(&BridgeResponse::err("capture", error))
        .unwrap_or_else(|_| "{\"v\":1,\"ok\":false,\"op\":\"capture\"}".to_string())
}

/// One connected bridge client, with its buffered input and output.
struct BridgeConnection {
    stream: UnixStream,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    /// A dup of the stream's fd, kept so the standalone backend can
    /// watch the connection for readability (see
    /// [`BridgeServer::connection_watch_fds`]). It is a separate fd
    /// number from the stream's, so closing the stream in `poll`
    /// cannot recycle the number under a registered calloop source.
    watch_fd: OwnedFd,
}

/// Try to flush one connection's outbox (non-blocking). Returns true
/// if the connection is dead and must be dropped. A partial write
/// drains what went out and leaves the rest for the next poll;
/// WouldBlock is not an error.
fn flush_connection(connection: &mut BridgeConnection) -> bool {
    if connection.outgoing.is_empty() {
        return false;
    }
    match connection.stream.write(&connection.outgoing) {
        Ok(written) => {
            connection.outgoing.drain(..written);
            false
        }
        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    }
}

/// The bridge socket server. Shared by the nested and standalone
/// backends (both serve the same Runtime channel).
pub(crate) struct BridgeServer {
    listener: UnixListener,
    connections: Vec<BridgeConnection>,
    /// The socket path, for removal at shutdown.
    pub(crate) path: PathBuf,
    /// The Runtime pid passed with `--runtime-pid`, if any. When set,
    /// the bridge admits ONLY that pid (see `poll`); when unset, the
    /// Stage 2/3 same-uid auth stands.
    runtime_pid: Option<u32>,
    /// True once a Runtime connection has ever been accepted. The
    /// degraded transition (last connection drops) only fires after
    /// one has existed — a compositor that has never seen the Runtime
    /// is not degraded, it is just waiting.
    had_connection: bool,
}

impl BridgeServer {
    pub(crate) fn bind(
        runtime_dir: &std::path::Path,
        runtime_pid: Option<u32>,
    ) -> std::io::Result<Self> {
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
            runtime_pid,
            had_connection: false,
        })
    }

    /// Accept pending connections (same-uid peers only; when
    /// `--runtime-pid` was given, only that pid) and serve one
    /// complete request line per connection per call. Never blocks.
    /// A duplicated fd for the listener, for calloop sources: the
    /// standalone backend polls this fd and calls [`Self::poll`] when it
    /// is readable. (A dup, not a borrow: the event loop owns the source
    /// while the server lives in the loop data.)
    pub(crate) fn poll_fd(&self) -> std::io::Result<std::os::unix::io::OwnedFd> {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.listener.as_raw_fd()) };
        borrowed.try_clone_to_owned()
    }

    /// One dup per live connection, for the standalone backend to
    /// watch: readability on a connection fd wakes the calloop loop,
    /// so a request on an existing connection is served promptly.
    /// Without this, the loop only wakes on the *listener* fd (a new
    /// connection) and a request sits until some other source — a
    /// pointer move, a client message — happens to fire. That is
    /// exactly what the first standalone adoption run showed: every
    /// bridge request timed out at the Runtime while the compositor
    /// only processed it once a QMP pointer move woke the loop.
    /// Each dup is a separate fd number from its stream's, keyed by
    /// the dup's own number (see `drm.rs`): closing the stream in
    /// `poll` cannot recycle the number under a registered source,
    /// and a fresh connection always gets a fresh dup number.
    pub(crate) fn connection_watch_fds(&self) -> Vec<OwnedFd> {
        self.connections
            .iter()
            .filter_map(|connection| connection.watch_fd.try_clone().ok())
            .collect()
    }

    /// Accept pending connections and serve one complete request line
    /// per connection per call. Never blocks. Also drains the state's
    /// queued bridge events (currently `chromium_died`) into every
    /// connected client, and tracks the Runtime's presence: when the
    /// last connection drops, the state goes degraded (the compositor
    /// keeps presenting the last scene but refuses new adoptions);
    /// when a connection arrives while degraded, the state recovers —
    /// the connection itself is the Runtime's re-registration (there
    /// is no separate register op in the v1 protocol).
    pub(crate) fn poll(&mut self, state: &mut GenesisState) {
        // Drain queued events first, so a `chromium_died` queued by
        // reconciliation in this same round reaches the Runtime on
        // this poll. Events are kept (capped) while no client is
        // connected, so a reconnecting Runtime still learns of them.
        if !self.connections.is_empty() && !state.pending_bridge_events.is_empty() {
            let events: Vec<String> = std::mem::take(&mut state.pending_bridge_events);
            for event in events {
                let mut bytes = event.into_bytes();
                bytes.push(b'\n');
                for connection in &mut self.connections {
                    connection.outgoing.extend_from_slice(&bytes);
                }
            }
        }
        if state.pending_bridge_events.len() > 16 {
            let excess = state.pending_bridge_events.len() - 16;
            state.pending_bridge_events.drain(..excess);
            diag!("bridge: dropped {excess} undelivered bridge event(s)");
        }
        // A resolved capture's reply, drained the same way as the
        // queued events: the handler records the request (see
        // `handle_bridge_request`), the backend resolves it after the
        // next render and stores the serialised reply in
        // `state.capture_reply`, and the next poll delivers it. The
        // reply goes to every connected client — in practice the one
        // Runtime that asked.
        if !self.connections.is_empty() {
            if let Some(reply) = state.capture_reply.take() {
                let mut bytes = reply.into_bytes();
                bytes.push(b'\n');
                for connection in &mut self.connections {
                    connection.outgoing.extend_from_slice(&bytes);
                }
            }
        }

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Trusted channel: the peer must be the same user.
                    // When `--runtime-pid` was given, the peer must
                    // additionally be exactly that pid — no other
                    // same-uid process may adopt surfaces.
                    let cred = peer_cred(&stream);
                    let allowed = cred
                        .map(|cred| {
                            cred.uid == unsafe { libc::getuid() }
                                && self
                                    .runtime_pid
                                    .map(|pid| cred.pid as u32 == pid)
                                    .unwrap_or(true)
                        })
                        .unwrap_or(false);
                    if !allowed {
                        if let Some(pid) = cred.map(|cred| cred.pid) {
                            diag!(
                                "bridge: rejected a connection from pid={pid} (expected runtime pid={})",
                                self.runtime_pid
                                    .map(|pid| pid.to_string())
                                    .unwrap_or_else(|| "(any same-uid)".to_string())
                            );
                        } else {
                            diag!("bridge: rejected a connection from another uid");
                        }
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if self.connections.len() >= 8 {
                        diag!("bridge: too many connections, dropping one");
                        continue;
                    }
                    if state.runtime_degraded {
                        state.runtime_degraded = false;
                        crate::marker!(
                            "RUNTIME RECONNECTED degraded cleared: new adoptions accepted again"
                        );
                    }
                    self.had_connection = true;
                    // A dup of the stream's fd for the standalone
                    // backend to watch (see `connection_watch_fds`).
                    // The dup is taken here, at accept, so its number
                    // is stable for the connection's lifetime.
                    let watch_fd = {
                        let borrowed = unsafe { BorrowedFd::borrow_raw(stream.as_raw_fd()) };
                        match borrowed.try_clone_to_owned() {
                            Ok(fd) => fd,
                            Err(_) => continue,
                        }
                    };
                    self.connections.push(BridgeConnection {
                        stream,
                        incoming: Vec::new(),
                        outgoing: Vec::new(),
                        watch_fd,
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
                if flush_connection(connection) {
                    drop_connection = true;
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
                // One complete line is one request. A `capture`
                // request has no immediate reply: the handler records
                // it and the backend resolves it after the next
                // render (see `handle_bridge_request`), so `None`
                // here means "reply deferred", not "no reply".
                if !drop_connection {
                    if let Some(newline) = connection.incoming.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = connection.incoming.drain(..=newline).collect();
                        response = handle_bridge_request(state, &line);
                    }
                }
            }
            if let Some(response) = response {
                let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
                bytes.push(b'\n');
                self.connections[index].outgoing.extend_from_slice(&bytes);
                // Flush the reply in this same poll instead of leaving
                // it for the next one: the next poll only runs when
                // some source fires, and a request that triggers no
                // render or input would leave its reply stuck in the
                // outbox until the Runtime does something else (the
                // first standalone adoption run showed exactly that:
                // a `close`'s {"ok":true} never arrived). A partial
                // write or WouldBlock leaves the rest for the next
                // poll's flush.
                if flush_connection(&mut self.connections[index]) {
                    drop_connection = true;
                }
            }
            if drop_connection {
                self.connections.remove(index);
            } else {
                index += 1;
            }
        }

        // The Runtime is gone: no bridge client remains. The
        // compositor keeps presenting the last scene, but new
        // adoptions are refused until the Runtime reconnects and
        // re-registers (see `poll`'s doc comment) — a degraded state,
        // marked, not a silent hang.
        if self.connections.is_empty() && self.had_connection && !state.runtime_degraded {
            state.runtime_degraded = true;
            crate::marker!(
                "RUNTIME DISCONNECTED degraded: refusing new adoptions until the Runtime reconnects"
            );
        }
    }
}

/// Handle one bridge request line. `close` is the Shell asking the
/// compositor to close an application (ADR-0006 milestone 9); `ping` is
/// the liveness check. Stage 4 adds `adopt` / `move` / `resize` /
/// `release` (see `adoption.rs` for the semantics and the exact error
/// strings). Screen capture adds `capture` (see `capture.rs`): it
/// records the request in `state.capture_request` and returns `None`
/// — the reply is deferred until the backend resolves the capture
/// after the next render. Every other op returns its reply
/// immediately (`Some`). A malformed request — bad JSON, a missing
/// required field, a non-positive rect dimension — is `"malformed
/// request"`; the adoption failures themselves carry their own
/// strings. A sensitive surface visible at request time refuses the
/// capture at once (`"refused: sensitive surface"`); the backend
/// re-checks at capture time, so a surface that appears between the
/// request and the render is still caught.
fn handle_bridge_request(state: &mut GenesisState, line: &[u8]) -> Option<BridgeResponse> {
    let request: BridgeRequest = match serde_json::from_slice(line) {
        Ok(request) => request,
        Err(_) => return Some(BridgeResponse::err("(unparsed)", "malformed request")),
    };
    if request.v != BRIDGE_PROTOCOL_VERSION {
        return Some(BridgeResponse::err(
            &request.op,
            "unsupported protocol version",
        ));
    }
    match request.op.as_str() {
        "ping" => Some(BridgeResponse::ok("ping", None)),
        "capture" => {
            if state.capture_request.is_some() {
                return Some(BridgeResponse::err("capture", "capture already pending"));
            }
            let output_path = match &request.path {
                Some(path) if !path.is_empty() => {
                    let path = std::path::PathBuf::from(path);
                    if !path.is_absolute() {
                        return Some(BridgeResponse::err("capture", "malformed request"));
                    }
                    path
                }
                Some(_) => return Some(BridgeResponse::err("capture", "malformed request")),
                None => std::path::PathBuf::from(crate::capture::DEFAULT_CAPTURE_PATH),
            };
            if let Some(detail) = crate::capture::sensitive_surface_visible(state) {
                crate::marker!("CAPTURE REFUSED reason=sensitive-surface detail={detail}");
                return Some(BridgeResponse::err("capture", "refused: sensitive surface"));
            }
            state.capture_request = Some(crate::capture::CaptureRequest { output_path });
            state.needs_redraw = true;
            None
        }
        "close" => {
            let Some(session) = request.session else {
                return Some(BridgeResponse::err("close", "malformed request"));
            };
            let Some(record) = state
                .registry
                .by_session(&SessionId(session.clone()))
                .map(|record| record.id)
            else {
                return Some(BridgeResponse::err("close", "unknown session"));
            };
            state.close_surface(record);
            state.needs_redraw = true;
            Some(BridgeResponse::ok("close", Some(session)))
        }
        "adopt" => {
            let (Some(session), Some(chromium_pid), Some(rect)) =
                (request.session, request.chromium_pid, request.rect)
            else {
                return Some(BridgeResponse::err("adopt", "malformed request"));
            };
            if rect.w <= 0 || rect.h <= 0 {
                return Some(BridgeResponse::err("adopt", "malformed request"));
            }
            match state.adopt(
                &session,
                chromium_pid,
                crate::adoption::ContentRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                },
            ) {
                Ok(()) => Some(BridgeResponse::ok("adopt", Some(session))),
                Err(error) => Some(BridgeResponse::err("adopt", error)),
            }
        }
        "move" => {
            let (Some(session), Some(rect)) = (request.session, request.rect) else {
                return Some(BridgeResponse::err("move", "malformed request"));
            };
            if rect.w <= 0 || rect.h <= 0 {
                return Some(BridgeResponse::err("move", "malformed request"));
            }
            match state.move_adoption(
                &session,
                crate::adoption::ContentRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                },
            ) {
                Ok(()) => Some(BridgeResponse::ok("move", Some(session))),
                Err(error) => Some(BridgeResponse::err("move", error)),
            }
        }
        "resize" => {
            let (Some(session), Some(rect)) = (request.session, request.rect) else {
                return Some(BridgeResponse::err("resize", "malformed request"));
            };
            if rect.w <= 0 || rect.h <= 0 {
                return Some(BridgeResponse::err("resize", "malformed request"));
            }
            match state.resize_adoption(
                &session,
                crate::adoption::ContentRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                },
            ) {
                Ok(()) => Some(BridgeResponse::ok("resize", Some(session))),
                Err(error) => Some(BridgeResponse::err("resize", error)),
            }
        }
        "release" => {
            let Some(session) = request.session else {
                return Some(BridgeResponse::err("release", "malformed request"));
            };
            match state.release_adoption(&session) {
                Ok(()) => Some(BridgeResponse::ok("release", Some(session))),
                Err(error) => Some(BridgeResponse::err("release", error)),
            }
        }
        _ => Some(BridgeResponse::err(&request.op, "unknown op")),
    }
}
