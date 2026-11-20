// SPDX-License-Identifier: GPL-3.0-or-later
//! Nested backend — the winit backend, the main loop, and the bridge socket.
//!
//! ## Backend pick: Smithay's winit backend
//!
//! Smithay's own reference compositor (`anvil`) and its `minimal` example
//! run nested the same way: the compositor is a Wayland client of the
//! parent compositor, opening one window through `winit` and rendering
//! into it with the GLES renderer. The winit backend is Smithay's
//! documented nested route (`backend::winit`: "running a compositor as a
//! Wayland or X11 client using winit") — no new backend code is written
//! here, and no DRM/KMS or libinput is involved in Stage 2. That is the
//! point of nesting: Stage 2 proves the server side (protocols, surfaces,
//! input routing, association) while the parent compositor owns the
//! hardware. Standalone (DRM/KMS + libinput) is Stage 3.
//!
//! ## The loop
//!
//! Each iteration: pump winit events (input, resize, close) → accept new
//! Wayland clients → dispatch + flush clients → drain client events →
//! reconcile the surface registry → serve bridge requests → render when
//! something changed → submit. The loop sleeps briefly when idle; Stage 2
//! makes no frame-rate claim (the GF65 is the fps proof, Stage 5).
//!
//! ## Bridge socket
//!
//! The Runtime↔compositor channel (see `runtime_bridge.rs`): a Unix
//! socket at `$XDG_RUNTIME_DIR/sevyn-compositor.sock`, mode 0600,
//! newline-delimited JSON, protocol version 1. Stage 2 ops: `ping` and
//! `close`. The peer must have the same uid as the compositor
//! (`SO_PEERCRED`); pid-binding to a known Runtime pid is Stage 4.
//!
//! ## Shutdown
//!
//! SIGTERM / SIGINT or the winit window's close request: ask every surface
//! to close, flush the clients, emit `SHUTDOWN clean`, and exit 0. The
//! compositor never spawns application processes, so there is nothing of
//! its own to orphan — and the nested check asserts no test client
//! survives the compositor's exit.

use std::{
    io::{Read, Write},
    os::unix::{
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use smithay::{
    backend::{
        renderer::{
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            utils::draw_render_elements,
            Color32F, Frame, Renderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::PopupManager,
    output::Mode,
    reexports::{
        wayland_server::{protocol::wl_surface::WlSurface, Display, ListeningSocket},
        winit::{
            dpi::LogicalSize, platform::pump_events::PumpStatus, window::Window as WinitWindow,
        },
    },
    utils::{Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor::{with_surface_tree_downward, SurfaceAttributes, TraversalAction},
        seat::WaylandFocus,
    },
};

use crate::{
    diag, input, marker,
    runtime_bridge::{BRIDGE_PROTOCOL_VERSION, BRIDGE_SOCKET_NAME},
    server::{ClientEvent, ClientState, GenesisState},
};

/// Set by the SIGTERM/SIGINT handler (async-signal-safe: a single atomic
/// store). The main loop polls it each iteration.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

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

/// The bridge socket server.
struct BridgeServer {
    listener: UnixListener,
    connections: Vec<BridgeConnection>,
    /// The socket path, for removal at shutdown.
    path: PathBuf,
}

impl BridgeServer {
    fn bind(runtime_dir: &std::path::Path) -> std::io::Result<Self> {
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
    fn poll(&mut self, state: &mut GenesisState) {
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
                .by_session(&crate::runtime_bridge::SessionId(session.clone()))
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

/// Send `wl_surface.frame` callbacks for a surface tree (copied from
/// Smithay's `minimal` example: the compositor must send frame callbacks
/// after presenting, or clients stall waiting for the next frame).
fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surface, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

/// Present one frame: render the scene into the winit window, send frame
/// callbacks, and submit.
fn render(
    state: &mut GenesisState,
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    {
        // The framebuffer borrows the backend (its `Drop` runs the GL
        // teardown), so the whole render must finish — and the borrow
        // must end — before `submit` borrows the backend again.
        let (renderer, mut framebuffer) = backend.bind()?;
        let space_elements =
            state
                .space
                .render_elements_for_output(renderer, &state.output, 1.0)?;

        // Popups render above their parent toplevel.
        let mut popup_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        for window in state.space.elements() {
            let Some(window_location) = state.space.element_location(window) else {
                continue;
            };
            let Some(wl_surface) = window.wl_surface() else {
                continue;
            };
            for (popup, popup_location) in PopupManager::popups_for_surface(&wl_surface) {
                let location = window_location + popup_location;
                popup_elements.extend(render_elements_from_surface_tree(
                    renderer,
                    popup.wl_surface(),
                    Point::<i32, Physical>::from((location.x, location.y)),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                ));
            }
        }

        let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
        frame.clear(Color32F::new(0.05, 0.05, 0.08, 1.0), &[damage])?;
        draw_render_elements(&mut frame, 1.0, &space_elements, &[damage])?;
        draw_render_elements(&mut frame, 1.0, &popup_elements, &[damage])?;
        let _ = frame.finish()?;
    }

    let time = state.start_time.elapsed().as_millis() as u32;
    for surface in state.xdg_shell_state.toplevel_surfaces() {
        send_frames_surface_tree(surface.wl_surface(), time);
    }

    // The parent compositor does the vsync for us.
    backend.submit(Some(&[damage]))?;

    if state.frames_presented == 0 {
        marker!("FIRST FRAME PRESENTED size={}x{}", size.w, size.h);
    }
    state.frames_presented += 1;
    state.needs_redraw = false;
    Ok(())
}

/// Run the nested compositor until shutdown. Returns `Ok(())` on a clean
/// exit (signal or window close); errors are fatal startup/runtime
/// failures and are reported by `main`.
pub fn run(
    socket_name: &str,
    output_size: Size<i32, Logical>,
) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_err(|_| "XDG_RUNTIME_DIR is not set; the Wayland socket has nowhere to live")?;

    let mut display: Display<GenesisState> = Display::new()?;
    let mut dh = display.handle();

    let mut state = GenesisState::new(dh.clone(), output_size);

    let listener = ListeningSocket::bind(socket_name)?;
    let mut clients = Vec::new();

    let (mut backend, mut winit) = winit::init_from_attributes::<GlesRenderer>(
        WinitWindow::default_attributes()
            .with_inner_size(LogicalSize::new(output_size.w as f64, output_size.h as f64))
            .with_title("SevynOS — Genesis (nested)")
            .with_visible(true),
    )?;

    let mut bridge = BridgeServer::bind(&runtime_dir)?;

    // Children (the Shell, test clients) connect through this socket.
    std::env::set_var("WAYLAND_DISPLAY", socket_name);

    marker!(
        "READY socket={socket_name} bridge={} output={}x{} backend=winit",
        bridge.path.display(),
        output_size.w,
        output_size.h
    );

    let mut close_requested = false;

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) || close_requested {
            break;
        }

        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                // The parent compositor resized our window: the output
                // follows it (scale stays 1 in Stage 2).
                let logical = Size::from((size.w, size.h));
                state.output_size = logical;
                state.output.change_current_state(
                    Some(Mode {
                        size: Size::<i32, Physical>::from((size.w, size.h)),
                        refresh: 60000,
                    }),
                    None,
                    None,
                    None,
                );
                state.space.refresh();
                state.needs_redraw = true;
            }
            WinitEvent::Input(event) => input::dispatch_input_event(&mut state, event),
            WinitEvent::CloseRequested => {
                close_requested = true;
            }
            WinitEvent::Redraw => {
                state.needs_redraw = true;
            }
            _ => {}
        });

        match status {
            PumpStatus::Continue => {}
            PumpStatus::Exit(_) => break,
        }

        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) || close_requested {
            break;
        }

        if let Some(stream) = listener.accept()? {
            let data = Arc::new(ClientState {
                events: state.client_events.clone(),
                ..Default::default()
            });
            match dh.insert_client(stream, data) {
                Ok(client) => {
                    // The pid is only known once the client exists: read
                    // it from the connection credentials and stash it in
                    // the client data, so `disconnected` can report it.
                    let pid = client
                        .get_credentials(&dh)
                        .ok()
                        .map(|credentials| credentials.pid as u32);
                    if let Some(pid) = pid {
                        if let Some(client_state) = client.get_data::<ClientState>() {
                            if let Ok(mut slot) = client_state.pid.lock() {
                                *slot = Some(pid);
                            }
                        }
                    }
                    if let Ok(mut events) = state.client_events.lock() {
                        events.push(ClientEvent::Connected { pid });
                    }
                    clients.push(client);
                }
                Err(err) => diag!("failed to accept a Wayland client: {err}"),
            }
        }

        display.dispatch_clients(&mut state)?;
        display.flush_clients()?;

        state.drain_client_events();
        state.reconcile_surfaces();
        bridge.poll(&mut state);

        if state.needs_redraw {
            if let Err(err) = render(&mut state, &mut backend) {
                diag!("render failed: {err}");
            }
        }

        // Idle sleep: the loop is event-driven (winit pump + client
        // dispatch), not a frame pump. 2 ms keeps input latency low
        // without busy-spinning.
        std::thread::sleep(Duration::from_millis(2));
    }

    // Clean shutdown: ask every application to close, flush the requests,
    // then exit. The nested check asserts no client survives us.
    state.close_all_surfaces();
    let _ = display.dispatch_clients(&mut state);
    let _ = display.flush_clients();
    let _ = std::fs::remove_file(&bridge.path);
    marker!("SHUTDOWN clean");
    Ok(())
}
