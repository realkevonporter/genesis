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
//! (`SO_PEERCRED`); with `--runtime-pid` (Stage 4) only that pid is
//! admitted. Stage 4 adds the adoption ops `adopt` / `move` / `resize`
//! / `release` on the same socket.
//!
//! ## Shutdown
//!
//! SIGTERM / SIGINT or the winit window's close request: ask every surface
//! to close, flush the clients, emit `SHUTDOWN clean`, and exit 0. The
//! compositor never spawns application processes, so there is nothing of
//! its own to orphan — and the nested check asserts no test client
//! survives the compositor's exit.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use smithay::{
    backend::{
        renderer::{gles::GlesRenderer, utils::draw_render_elements, Color32F, Frame, Renderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::Mode,
    reexports::{
        wayland_server::{Display, ListeningSocket},
        winit::{
            dpi::LogicalSize, platform::pump_events::PumpStatus, window::Window as WinitWindow,
        },
    },
    utils::{Logical, Physical, Rectangle, Size, Transform},
};

use crate::{
    diag, input, marker,
    render::{
        adopted_elements, popup_elements, send_frames, send_presentation_feedback, splice_adopted,
        NestedRenderElements,
    },
    runtime_bridge::BridgeServer,
    server::{ClientEvent, ClientState, GenesisState},
};

/// Set by the SIGTERM/SIGINT handler (async-signal-safe: a single atomic
/// store). The main loop polls it each iteration.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Present one frame: render the scene into the winit window, send frame
/// callbacks, and submit.
///
/// Stage 4: the scene is the space's elements with each adoption's
/// clipped Chromium elements spliced in immediately above its Browser
/// window's elements (see `render::splice_adopted`), drawn in one pass
/// — so a Browser move and its adopted element's move are the same
/// frame, structurally.
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
        let mut elements: Vec<NestedRenderElements<GlesRenderer>> = space_elements
            .into_iter()
            .map(|element| match element {
                smithay::desktop::space::SpaceRenderElements::Surface(element) => {
                    NestedRenderElements::Surface(element)
                }
                smithay::desktop::space::SpaceRenderElements::Element(element) => {
                    NestedRenderElements::Wrapped(element)
                }
                smithay::desktop::space::SpaceRenderElements::_GenericCatcher(_) => {
                    unreachable!()
                }
            })
            .collect();
        let adopted: Vec<(
            smithay::desktop::Window,
            Vec<NestedRenderElements<GlesRenderer>>,
        )> = adopted_elements(state, renderer, 1.0)
            .into_iter()
            .map(|(window, cropped)| {
                (
                    window,
                    cropped
                        .into_iter()
                        .map(NestedRenderElements::Cropped)
                        .collect(),
                )
            })
            .collect();
        elements = splice_adopted(elements, adopted);
        let popup_elements = popup_elements(state, renderer, 1.0);

        let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
        frame.clear(Color32F::new(0.05, 0.05, 0.08, 1.0), &[damage])?;
        draw_render_elements(&mut frame, 1.0, &elements, &[damage])?;
        draw_render_elements(&mut frame, 1.0, &popup_elements, &[damage])?;
        let _ = frame.finish()?;
    }

    let time = state.start_time.elapsed().as_millis() as u32;
    send_frames(state, time);

    // The parent compositor does the vsync for us.
    backend.submit(Some(&[damage]))?;

    // Presentation-time feedback for surfaces that asked for it (only
    // for a frame that was actually submitted).
    send_presentation_feedback(state);

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
    runtime_pid: Option<u32>,
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

    let mut bridge = BridgeServer::bind(&runtime_dir, runtime_pid)?;

    // Children (the Shell, test clients) connect through this socket.
    std::env::set_var("WAYLAND_DISPLAY", socket_name);

    marker!(
        "READY socket={socket_name} bridge={} output={}x{} backend=winit runtime_pid={}",
        bridge.path.display(),
        output_size.w,
        output_size.h,
        runtime_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "(any same-uid)".to_string())
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

        // Idle: pump the notifier's timers and log DPMS transitions.
        // Nested mode owns no panel, so a transition is a no-op that
        // is logged once (the parent compositor owns DPMS); the state
        // still tracks idle/wake so the markers stay truthful.
        state.pump_idle_timers();
        match state.poll_idle() {
            Some(false) => {
                diag!("idle: display would power off (nested mode: DPMS is a no-op)")
            }
            Some(true) => {
                diag!("input: display wake (nested mode: DPMS is a no-op)")
            }
            None => {}
        }

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
