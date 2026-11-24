// SPDX-License-Identifier: GPL-3.0-or-later
//! Server state — the Smithay `State` and its protocol handlers.
//!
//! [`GenesisState`] owns everything the compositor serves: the Wayland
//! globals (compositor, shm, output, seat, xdg-shell), the single output,
//! the `Space<Window>` scene, the surface registry, and the input handles.
//!
//! Design rules enforced here (see COMPOSITOR_PLAN.md):
//!
//! * **One output.** Stage 2 has exactly one output, mapped into the
//!   scene at (0, 0). Multi-output is an explicit follow-up.
//! * **Compositor-owned placement.** Windows are placed by
//!   [`GenesisState::place_window`] (cascade); clients never position
//!   themselves. Stage 4 adoption moves elements in this same scene.
//! * **No invented policy.** Focus follows the documented Stage 2 default
//!   (see `input.rs`): new toplevel takes focus; pointer press raises and
//!   focuses. The Shell's own focus policy arrives with the Shell.
//! * **No `wlr_*` / C core.** Smithay 0.7 only, Rust end to end.

use std::sync::{Arc, Mutex};

use calloop::EventLoop;
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    delegate_compositor, delegate_content_type, delegate_cursor_shape, delegate_dmabuf,
    delegate_foreign_toplevel_list, delegate_idle_inhibit, delegate_idle_notify,
    delegate_layer_shell, delegate_output, delegate_presentation, delegate_seat, delegate_shm,
    delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_activation, delegate_xdg_shell,
    desktop::{layer_map_for_output, LayerSurface, PopupManager, Space, Window, WindowSurfaceType},
    input::{
        pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
        touch::TouchHandle,
        Seat, SeatHandler, SeatState,
    },
    reexports::wayland_server::{
        backend::{ClientData, ClientId, DisconnectReason},
        delegate_dispatch, delegate_global_dispatch,
        protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_surface::WlSurface},
        Client, DisplayHandle, Resource,
    },
    utils::{Logical, Physical, Point, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{with_states, CompositorClientState, CompositorHandler, CompositorState},
        content_type::ContentTypeState,
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState},
        foreign_toplevel_list::ForeignToplevelListState,
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        output::{OutputHandler, OutputManagerState},
        presentation::PresentationState,
        seat::WaylandFocus,
        shell::{
            wlr_layer::{
                Layer as WlrLayer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::XdgShellState,
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletSeatHandler,
        viewporter::ViewporterState,
        xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken},
    },
};

use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
};

use crate::{
    adoption::Adoption,
    cursor::build_default_cursor_buffer,
    drm::DrmRenderer,
    foreign_toplevel::{ForeignToplevelManagerGlobalData, ForeignToplevelManagerState},
    marker, output as output_mod,
    runtime_bridge::{self, SessionId},
    surface_registry::SurfaceRegistry,
};

/// Events a client reports through its [`ClientData`]. Drained by the main
/// loop after each client dispatch (see `backend.rs`).
#[derive(Debug, Clone)]
pub enum ClientEvent {
    Connected { pid: Option<u32> },
    Disconnected { pid: Option<u32> },
}

/// Per-client data attached to every Wayland connection.
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Client pid from the connection credentials. Set by the main loop
    /// right after insertion (the pid is only knowable once the client
    /// exists); read by `disconnected`.
    pub pid: Mutex<Option<u32>>,
    /// Shared queue back to the compositor state.
    pub events: Arc<Mutex<Vec<ClientEvent>>>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            compositor_state: CompositorClientState::default(),
            pid: Mutex::new(None),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {
        // The loop reports `Connected` itself once the pid is known
        // (it is not knowable here yet), so there is nothing to do.
    }

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        let pid = self.pid.lock().ok().and_then(|slot| *slot);
        if let Ok(mut events) = self.events.lock() {
            events.push(ClientEvent::Disconnected { pid });
        }
    }
}

/// The compositor state. Single-threaded: only the main loop touches it.
pub struct GenesisState {
    pub dh: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    /// Retained even though nothing reads it: the xdg-output-manager
    /// global is created through this state, and keeping the state alive
    /// keeps the constructor's ownership story explicit (a future Smithay
    /// `Drop` that unadvertised the global would otherwise silently
    /// break clients that already bound it).
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    /// Stage 3 protocol states. Each creates one global at startup and
    /// is retained for the same reason as `output_manager_state` (see
    /// `compositor/README.md` for the list and what each one does).
    /// None of these is read after construction except the idle
    /// notifier (see `idle.rs`) and the layer shell (see
    /// `WlrLayerShellHandler` below).
    #[allow(dead_code)]
    pub layer_shell_state: WlrLayerShellState,
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub content_type_state: ContentTypeState,
    #[allow(dead_code)]
    pub single_pixel_buffer_state: SinglePixelBufferState,
    #[allow(dead_code)]
    pub xdg_activation_state: XdgActivationState,
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    /// Stage 4: `ext-foreign-toplevel-list-v1` (Smithay ships it) and
    /// the Genesis-implemented `zwlr_foreign_toplevel_manager_v1`
    /// global (see `foreign_toplevel.rs`). Each creates one global
    /// at startup and is retained for the same reason as
    /// `output_manager_state` above.
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    pub foreign_toplevel_manager_state: ForeignToplevelManagerState,

    pub seat: Seat<Self>,
    /// The seat's keyboard handle (created once; Stage 2 has one seat).
    pub keyboard: smithay::input::keyboard::KeyboardHandle<Self>,
    /// The seat's pointer handle.
    pub pointer: smithay::input::pointer::PointerHandle<Self>,
    /// The seat's touch handle (Stage 3: libinput touch; nested mode
    /// never delivers touch events, so this stays idle there).
    pub touch: TouchHandle<Self>,

    /// The scene. Genesis owns the render list and positions each
    /// surface's element — that is the adoption mechanism (Stage 4).
    pub space: Space<Window>,
    /// The one output (Stage 2: exactly one).
    pub output: smithay::output::Output,
    /// The output size in logical pixels (== physical at scale 1).
    pub output_size: Size<i32, Logical>,
    /// The integer output scale (1 in nested mode; `--scale` standalone).
    /// Logical size = transformed physical size / scale.
    pub output_scale: i32,
    /// The output transform in effect (Normal in nested mode; the
    /// `--rotate` value in standalone mode). Absolute input coordinates
    /// are mapped through this (see `input.rs`).
    pub output_transform: Transform,
    /// The output's physical mode size, before scale/transform. Absolute
    /// input devices report in this space.
    pub output_physical_size: Size<i32, Physical>,

    pub popup_manager: PopupManager,
    pub registry: SurfaceRegistry,

    /// The dmabuf protocol state. Always present; the `zwp_linux_dmabuf_v1`
    /// global itself is only created by the standalone backend (see
    /// `drm.rs`), so nested mode advertises no dmabuf global.
    pub dmabuf_state: DmabufState,
    /// The advertised dmabuf global, if any. Kept alive while advertised:
    /// dropping it would unadvertise the global from live clients.
    pub dmabuf_global: Option<DmabufGlobal>,
    /// The standalone backend's active renderer (GLES primary, Pixman
    /// fallback), if any. Lives here — not in the backend — so the
    /// dmabuf handler can import client dmabufs into it. `None` in
    /// nested mode, where no dmabuf global exists and imports fail.
    pub drm_renderer: Option<DrmRenderer>,
    /// The client-set cursor surface and its hotspot, if a client set
    /// one (`wl_pointer.set_cursor`). The standalone backend renders it
    /// at the pointer location (hardware cursor plane when it fits, GL
    /// compositing otherwise). `None` means no client cursor: named
    /// cursors (`cursor-shape`) carry no buffer, and hidden means hidden
    /// — in the named case the built-in default arrow is drawn instead
    /// (see `cursor_named` / `default_cursor` below).
    pub cursor_surface: Option<(WlSurface, Point<i32, Logical>)>,
    /// The named cursor shape last requested via `cursor-shape`, if
    /// any (`CursorImageStatus::Named`). Every named shape renders as
    /// the same built-in arrow — there is no cursor theme system (see
    /// `cursor.rs`); this is recorded so the choice is visible, not
    /// so the renderer can vary by it.
    pub cursor_named: Option<CursorIcon>,
    /// True when the client hid the cursor (`CursorImageStatus::Hidden`):
    /// no cursor element at all is drawn, not even the default arrow.
    pub cursor_hidden: bool,
    /// The built-in default arrow (16x16, see `cursor.rs`), built once
    /// at startup. Drawn by the standalone backend whenever there is
    /// no client-set cursor surface and the cursor is not hidden, so
    /// the pointer is never invisible.
    pub default_cursor: MemoryRenderBuffer,

    /// Serials handed to clients for input/focus events.
    serial_counter: u32,
    /// Last pointer location in output (logical) coordinates.
    pub pointer_location: Point<f64, Logical>,
    /// Surface id under the pointer, if any.
    pub pointer_focus_id: Option<u64>,
    /// Namespace of the layer surface under the pointer, if any (see
    /// `input.rs` — a layer hit has no registry record, so the id
    /// stays `None` and the marker reports the namespace instead).
    pub pointer_focus_layer: Option<String>,
    /// Surface id holding keyboard focus, if any.
    pub keyboard_focus_id: Option<u64>,
    /// Set when a frame needs presenting (commit, new surface, resize…).
    pub needs_redraw: bool,
    /// Frames presented since start (for the `FIRST FRAME` marker).
    pub frames_presented: u64,
    /// Monotonic start time, for `wl_surface.frame` callbacks.
    pub start_time: std::time::Instant,

    /// The idle-notifier state (`ext-idle-notify-v1`). Its timers run
    /// on `idle_loop` (below) — see `idle.rs` for why the notifier
    /// has its own loop instead of the backends' loops.
    pub idle_notifier_state: IdleNotifierState<Self>,
    /// The dedicated calloop loop for the idle-notifier's timers.
    /// Pumped with a zero timeout on every backend iteration (see
    /// `idle.rs`, `pump_idle_timers`); it holds no other sources.
    pub idle_loop: Option<EventLoop<'static, Self>>,
    /// Surfaces currently holding an idle inhibitor (see `idle.rs`).
    /// Pruned of dead surfaces on every poll.
    pub idle_inhibitors: Vec<WlSurface>,
    /// Last input activity (any pointer/keyboard/touch event), for
    /// the DPMS idle clock.
    pub last_activity: std::time::Instant,
    /// True while the display is powered off for idle. Set when the
    /// backend applies a DPMS-off transition, cleared when input or
    /// a new inhibitor requests a wake.
    pub dpms_off: bool,
    /// A pending DPMS transition for the backend to apply (`Some(true)`
    /// = power on, `Some(false)` = power off). Set by input/inhibit
    /// (wake) and by `poll_idle` (idle); taken exactly once.
    pub wants_dpms: Option<bool>,

    /// Client connect/disconnect events, pushed by `ClientState`.
    pub client_events: Arc<Mutex<Vec<ClientEvent>>>,

    // --- Stage 4 adoption state (see `adoption.rs`). Kept in this
    // clearly separate region: the manager track owns the
    // protocol-state fields elsewhere in this struct, not these.
    /// Live adoptions: one per Browser window (see `adoption.rs`).
    pub adoptions: Vec<Adoption>,
    /// Bridge events queued for the Runtime (currently only
    /// `chromium_died`); drained into bridge connections by
    /// `BridgeServer::poll` (see `runtime_bridge.rs`).
    pub pending_bridge_events: Vec<String>,
    /// True after the Runtime's bridge connection drops, until it
    /// reconnects. While degraded the compositor keeps presenting
    /// the last scene but refuses new adoptions (see
    /// `GenesisState::adopt`).
    pub runtime_degraded: bool,
}

impl GenesisState {
    pub fn new(dh: DisplayHandle, output_size: Size<i32, Logical>) -> Self {
        let output = output_mod::create_output(&dh, output_size);
        Self::new_inner(
            dh,
            output,
            output_size,
            1,
            Transform::Normal,
            Size::from((output_size.w, output_size.h)),
        )
    }

    /// The standalone backend's constructor: the output is the panel's
    /// native mode at the given integer scale and static rotation.
    /// `refresh` is the mode's refresh rate in millihertz.
    pub fn new_standalone(
        dh: DisplayHandle,
        logical_size: Size<i32, Logical>,
        physical_size: Size<i32, Physical>,
        scale: i32,
        transform: Transform,
        physical_mm: Option<(u32, u32)>,
        refresh: i32,
    ) -> Self {
        let output = output_mod::create_output_with(
            &dh,
            physical_size,
            scale,
            transform,
            physical_mm,
            refresh,
            "Genesis DRM output",
        );
        Self::new_inner(dh, output, logical_size, scale, transform, physical_size)
    }

    fn new_inner(
        dh: DisplayHandle,
        output: smithay::output::Output,
        output_size: Size<i32, Logical>,
        output_scale: i32,
        output_transform: Transform,
        output_physical_size: Size<i32, Physical>,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "sevyn");
        let keyboard = seat
            .add_keyboard(Default::default(), 200, 200)
            .expect("seat keyboard");
        let pointer = seat.add_pointer();
        let touch = seat.add_touch();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        // Stage 3 protocol globals (see COMPOSITOR_PLAN.md, "Fully
        // equipped"). Each `new` creates its global on the display.
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, libc::CLOCK_MONOTONIC as u32);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let content_type_state = ContentTypeState::new::<Self>(&dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        // Stage 4: the list global (Smithay ships it) and the
        // Genesis-implemented manager global (see
        // `foreign_toplevel.rs`).
        let foreign_toplevel_list_state = ForeignToplevelListState::new::<Self>(&dh);
        let foreign_toplevel_manager_state = ForeignToplevelManagerState::new(&dh);
        // The idle-notifier's timers need a `LoopHandle<'static, Self>`,
        // which neither backend's loop can supply (standalone's loop is
        // `EventLoop<StandaloneLoop>`, nested has no calloop loop), so
        // the notifier gets a dedicated loop, pumped every iteration —
        // see `idle.rs`.
        let idle_loop: EventLoop<'static, Self> =
            EventLoop::try_new().expect("idle event loop: cannot create epoll instance");
        let idle_notifier_state = IdleNotifierState::new(&dh, idle_loop.handle());

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        Self {
            dh: dh.clone(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            output_manager_state,
            layer_shell_state,
            presentation_state,
            viewporter_state,
            content_type_state,
            single_pixel_buffer_state,
            xdg_activation_state,
            idle_inhibit_state,
            cursor_shape_state,
            foreign_toplevel_list_state,
            foreign_toplevel_manager_state,
            seat,
            keyboard,
            pointer,
            touch,
            space,
            output,
            output_size,
            output_scale,
            output_transform,
            output_physical_size,
            popup_manager: PopupManager::default(),
            registry: SurfaceRegistry::new(),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            drm_renderer: None,
            cursor_surface: None,
            cursor_named: None,
            cursor_hidden: false,
            default_cursor: build_default_cursor_buffer(),
            serial_counter: 1,
            pointer_location: (0.0, 0.0).into(),
            pointer_focus_id: None,
            pointer_focus_layer: None,
            keyboard_focus_id: None,
            needs_redraw: true,
            frames_presented: 0,
            start_time: std::time::Instant::now(),
            idle_notifier_state,
            idle_loop: Some(idle_loop),
            idle_inhibitors: Vec::new(),
            last_activity: std::time::Instant::now(),
            dpms_off: false,
            wants_dpms: None,
            client_events: Arc::new(Mutex::new(Vec::new())),
            adoptions: Vec::new(),
            pending_bridge_events: Vec::new(),
            runtime_degraded: false,
        }
    }

    /// The next serial for client-facing events.
    pub fn next_serial(&mut self) -> Serial {
        let serial = Serial::from(self.serial_counter);
        self.serial_counter = self.serial_counter.wrapping_add(1).max(1);
        serial
    }

    /// Drain client connect/disconnect events into markers. Called by the
    /// main loop after each client dispatch.
    pub fn drain_client_events(&mut self) {
        let events = self
            .client_events
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        for event in events {
            match event {
                ClientEvent::Connected { pid } => {
                    marker!(
                        "CLIENT CONNECTED pid={}",
                        pid.map(|p| p.to_string())
                            .unwrap_or_else(|| "(unknown)".to_string())
                    );
                }
                ClientEvent::Disconnected { pid } => {
                    marker!(
                        "CLIENT DISCONNECTED pid={}",
                        pid.map(|p| p.to_string())
                            .unwrap_or_else(|| "(unknown)".to_string())
                    );
                }
            }
        }
    }

    /// Compositor-owned placement: cascade new windows down-right from
    /// (0, 0), clamped inside the output. Clients never position
    /// themselves — see the module docs.
    pub fn place_window(&mut self, window: &Window) {
        const CASCADE_STEP: i32 = 48;
        let count = self.space.elements().count() as i32;
        let geometry = window.geometry();
        let max_x = (self.output_size.w - geometry.size.w).max(0);
        let max_y = (self.output_size.h - geometry.size.h).max(0);
        let x = (count * CASCADE_STEP).min(max_x);
        let y = (count * CASCADE_STEP).min(max_y);
        self.space.map_element(window.clone(), (x, y), false);
    }

    /// Give keyboard focus to a window (or clear it), updating the
    /// `Activated` state on the affected toplevels. Called on map and on
    /// pointer press (see `input.rs`).
    ///
    /// Stage 4 guard: an adopted Chromium surface is never focused
    /// directly — focus routes through its Browser window (see
    /// `GenesisState::focus_adopted`), so the `Activated` state stays
    /// on the Browser toplevel while keys go to the page.
    pub fn set_keyboard_focus(&mut self, window: Option<&Window>) {
        if let Some(window) = window {
            if let Some(adoption) = self.adoption_for_chromium_window(window) {
                let browser_surface_id = adoption.browser_surface_id;
                let browser = self
                    .registry
                    .get(browser_surface_id)
                    .map(|record| record.window.clone());
                if let Some(browser) = browser {
                    let chromium = window.clone();
                    self.focus_adopted(&browser, &chromium);
                    return;
                }
            }
        }
        let serial = self.next_serial();
        let target_id = window.and_then(|window| {
            self.registry
                .records()
                .iter()
                .find(|record| record.window == *window)
                .map(|record| record.id)
        });
        if target_id == self.keyboard_focus_id {
            return;
        }
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            let target_surface_id =
                window.and_then(|window| window.toplevel().map(|t| t.wl_surface().id()));
            let is_target = target_surface_id
                .as_ref()
                .is_some_and(|id| *id == toplevel.wl_surface().id());
            toplevel.with_pending_state(|state| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
                if is_target {
                    state.states.set(ToplevelState::Activated);
                } else {
                    state.states.unset(ToplevelState::Activated);
                }
            });
            toplevel.send_configure();
        }
        let focus =
            window.and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, focus, serial);
        self.keyboard_focus_id = target_id;
        self.needs_redraw = true;
    }

    /// Give keyboard focus to a layer surface (or clear it). Layers
    /// are not in the `Space` and hold no `Activated` state: every
    /// toplevel is deactivated and keyboard focus moves straight to
    /// the layer's leaf surface. Called on pointer press / touch down
    /// over a layer that can receive keyboard focus (see `input.rs`),
    /// and with `None` when a focused layer surface is destroyed.
    ///
    /// A layer holding focus is untouched by toplevel reconciliation:
    /// `reconcile_surfaces` only moves focus when
    /// `keyboard_focus_id == Some(id)` for a gone record, and a layer
    /// focus keeps that id at `None`.
    pub fn set_keyboard_focus_layer(&mut self, surface: Option<&WlSurface>) {
        let serial = self.next_serial();
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            toplevel.with_pending_state(|state| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
                state.states.unset(ToplevelState::Activated);
            });
            toplevel.send_configure();
        }
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface.cloned(), serial);
        self.keyboard_focus_id = None;
        self.needs_redraw = true;
    }

    /// Namespace of the mapped layer surface owning `surface`, if the
    /// surface belongs to a layer (toplevel or subsurface leaf).
    /// Used by `focus_changed` and `layer_destroyed` so the
    /// keyboard-focus marker names a layer instead of reporting
    /// `(none)`, and so a destroyed layer can tell whether it held
    /// keyboard focus.
    pub fn layer_namespace_for_surface(&self, surface: &WlSurface) -> Option<String> {
        layer_map_for_output(&self.output)
            .layer_for_surface(surface, WindowSurfaceType::ALL)
            .map(|layer_surface| layer_surface.namespace().to_string())
    }

    /// Ask a surface to close (bridge `close`, or shutdown). The client
    /// is expected to exit; if it disconnects instead, reconciliation
    /// still records the stopped state.
    pub fn close_surface(&mut self, id: u64) -> bool {
        let Some(record) = self.registry.get_mut(id) else {
            return false;
        };
        record.close_requested = true;
        record.toplevel.send_close();
        true
    }

    /// Ask every surface to close (clean shutdown path).
    pub fn close_all_surfaces(&mut self) {
        let ids: Vec<u64> = self
            .registry
            .records()
            .iter()
            .map(|record| record.id)
            .collect();
        for id in ids {
            self.close_surface(id);
        }
    }

    /// Reconcile the registry against the live toplevels. Called every
    /// loop iteration after dispatching clients:
    ///
    /// * refresh app id / title and geometry; emit `SURFACE COMMITTED`
    ///   when a surface first presents a buffer or changes size;
    /// * drop records whose toplevel is gone (closed or disconnected) and
    ///   emit `SESSION STOPPED` for them, moving keyboard focus to the
    ///   topmost remaining window when the focused one went away.
    pub fn reconcile_surfaces(&mut self) {
        let live: Vec<smithay::reexports::wayland_server::backend::ObjectId> = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .map(|toplevel| toplevel.wl_surface().id())
            .collect();

        // Refresh metadata + geometry for live surfaces.
        let live_ids: Vec<u64> = self
            .registry
            .records()
            .iter()
            .filter(|record| live.contains(&record.object_id))
            .map(|record| record.id)
            .collect();
        for id in live_ids {
            self.refresh_metadata(id);
            if let Some(size) = self.registry.refresh_geometry(id) {
                marker!("SURFACE COMMITTED id={id} size={}x{}", size.0, size.1);
                self.needs_redraw = true;
            }
        }

        // Drop records whose toplevel is gone.
        let gone: Vec<(u64, SessionId, bool)> = self
            .registry
            .records()
            .iter()
            .filter(|record| !live.contains(&record.object_id))
            .map(|record| (record.id, record.session.clone(), record.close_requested))
            .collect();
        // Stage 4: drop adoptions whose Browser or Chromium surface is
        // gone first — a dead Chromium notifies the Runtime with the
        // `chromium_died` event (see `adoption.rs`).
        let gone_ids: Vec<u64> = gone.iter().map(|(id, _, _)| *id).collect();
        self.reconcile_adoptions(&gone_ids);
        let mut focus_lost = false;
        for (id, session, close_requested) in gone {
            if self.keyboard_focus_id == Some(id) {
                focus_lost = true;
            }
            if let Some(record) = self.registry.remove(id) {
                self.space.unmap_elem(&record.window);
                let reason = if close_requested {
                    "closed"
                } else {
                    "disconnected"
                };
                runtime_bridge::disassociate(&session, id, reason);
            }
        }
        if focus_lost {
            self.keyboard_focus_id = None;
            let topmost = self.space.elements().next_back().cloned();
            self.set_keyboard_focus(topmost.as_ref());
        }

        // Stage 4: announce new toplevels to manager/list clients,
        // echo title/app_id/state changes, and close entries for
        // toplevels that went away (see `foreign_toplevel.rs`).
        self.sync_foreign_toplevels();
    }

    /// Refresh a record's app id / title from the toplevel state. Emits a
    /// marker when either changes (they arrive after `new_toplevel`, via
    /// `set_app_id` / `set_title`).
    fn refresh_metadata(&mut self, id: u64) {
        let Some(record) = self.registry.get(id) else {
            return;
        };
        let surface = record.wl_surface().clone();
        let (app_id, title) = with_states(&surface, |states| {
            let Some(data) = states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            else {
                return (String::new(), String::new());
            };
            let attributes = data.lock().unwrap();
            (
                attributes.app_id.clone().unwrap_or_default(),
                attributes.title.clone().unwrap_or_default(),
            )
        });
        let changed = {
            let Some(record) = self.registry.get_mut(id) else {
                return;
            };
            let changed = record.app_id != app_id || record.title != title;
            record.app_id = app_id;
            record.title = title;
            changed
        };
        if changed {
            let record = self.registry.get(id).unwrap();
            marker!(
                "SURFACE METADATA id={id} app_id={} title={}",
                if record.app_id.is_empty() {
                    "(none)"
                } else {
                    &record.app_id
                },
                if record.title.is_empty() {
                    "(none)"
                } else {
                    &record.title
                }
            );
        }
    }
}

impl BufferHandler for GenesisState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl DmabufHandler for GenesisState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// A client imported a dmabuf: import it into the standalone
    /// backend's active renderer so it can be sampled later. With no
    /// standalone renderer (nested mode, or before the DRM device is
    /// up) the import fails — clients then fall back to SHM, the same
    /// path the nested check already proves. A failed import here is
    /// per-buffer, never fatal to the compositor.
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        let imported = match self.drm_renderer.as_mut() {
            Some(renderer) => renderer.import_dmabuf(&dmabuf).is_ok(),
            None => false,
        };
        if imported {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(GenesisState);

impl CompositorHandler for GenesisState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client without ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        use smithay::backend::renderer::utils::on_commit_buffer_handler;
        on_commit_buffer_handler::<Self>(surface);
        // A commit anywhere in a window's tree can change its bounds.
        for window in self.space.elements() {
            window.on_commit();
        }
        // A layer surface's commit can change its size/anchors, which
        // changes its arranged geometry in the LayerMap. Re-arranging
        // on every commit is cheap (it walks the mapped layers, usually
        // none) and keeps the map correct without a separate hook.
        // `arrange` only stages the new geometry as pending state —
        // the client learns it from a configure, so forward any pending
        // configure now (a no-op when nothing changed). Without this
        // a layer client keeps the first configure it got: the initial
        // configure is sent from `new_layer_surface`, before the
        // client's `set_size` is even read, so with no requested size
        // yet the arrange falls back to half the output — and the
        // client would render at that fallback size forever, never
        // learning its real size. (The Shell would hit exactly this
        // when it asks for fullscreen.)
        layer_map_for_output(&self.output).arrange();
        for layer in self.layer_shell_state.layer_surfaces() {
            layer.send_pending_configure();
        }
        self.needs_redraw = true;
    }
}

impl ShmHandler for GenesisState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for GenesisState {}

impl SeatHandler for GenesisState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let label = focused.and_then(|surface| {
            self.registry
                .by_object_id(&surface.id())
                .map(|record| record.id.to_string())
                .or_else(|| {
                    self.layer_namespace_for_surface(surface)
                        .map(|namespace| format!("layer={namespace}"))
                })
        });
        marker!(
            "FOCUS KEYBOARD surface={}",
            label.unwrap_or_else(|| "(none)".to_string())
        );
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // Nested mode: the parent compositor draws the cursor over our
        // window, so this is only read by the standalone backend. A
        // surface cursor is stored with its hotspot; a named cursor
        // (`cursor-shape`) carries no buffer, so it clears the surface
        // and is recorded — the standalone backend then draws the
        // built-in default arrow for it (every named shape renders as
        // the same arrow; there is no cursor theme system, see
        // `cursor.rs`). Hidden clears both: nothing is drawn at all.
        match image {
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .and_then(|attributes| attributes.lock().ok())
                        .map(|attributes| attributes.hotspot)
                        .unwrap_or_default()
                });
                self.cursor_surface = Some((surface, hotspot));
                self.cursor_named = None;
                self.cursor_hidden = false;
            }
            CursorImageStatus::Named(icon) => {
                self.cursor_surface = None;
                self.cursor_named = Some(icon);
                self.cursor_hidden = false;
            }
            CursorImageStatus::Hidden => {
                self.cursor_surface = None;
                self.cursor_named = None;
                self.cursor_hidden = true;
            }
        }
        self.needs_redraw = true;
    }
}

impl WlrLayerShellHandler for GenesisState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    /// A new layer surface (dock, top bar, panel) was created. It is
    /// mapped into the output's LayerMap — the scene's
    /// `render_elements_for_output` already renders mapped layers
    /// (background/bottom below windows, top/overlay above), so
    /// mapping is the whole render story — and given an initial
    /// configure so the client knows how to size itself. Pointer and
    /// keyboard input are routed to layers by `input.rs` (z-ordered
    /// hit-test; a layer never raises, and takes keyboard focus only
    /// when it can receive it). The remaining step is the client
    /// side: the Shell migrating its chrome onto layer surfaces (a
    /// sevynos change, not a Genesis one).
    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        _output: Option<WlOutput>,
        layer: WlrLayer,
        namespace: String,
    ) {
        // Single output: the requested output is ignored; every layer
        // surface lands on the one output.
        let layer_surface = LayerSurface::new(surface, namespace.clone());
        let mapped = layer_map_for_output(&self.output)
            .map_layer(&layer_surface)
            .is_ok();
        layer_surface.layer_surface().send_configure();
        marker!("LAYER SURFACE namespace={namespace} layer={layer:?} mapped={mapped}");
        self.needs_redraw = true;
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let wl_surface = surface.wl_surface().clone();
        let existing = layer_map_for_output(&self.output)
            .layer_for_surface(&wl_surface, WindowSurfaceType::TOPLEVEL)
            .cloned();
        if let Some(layer_surface) = existing {
            // A destroyed layer that held keyboard focus must release
            // it, or keys would keep going to a dead surface. The
            // focus may sit on a subsurface leaf of the layer, so the
            // check resolves the focused surface through the layer
            // map (not by object id) — and runs before the unmap.
            let had_focus = self
                .keyboard
                .current_focus()
                .and_then(|focused| self.layer_namespace_for_surface(&focused))
                .is_some_and(|namespace| namespace == layer_surface.namespace());
            if had_focus {
                self.set_keyboard_focus_layer(None);
            }
            let namespace = layer_surface.namespace().to_string();
            layer_map_for_output(&self.output).unmap_layer(&layer_surface);
            marker!("LAYER SURFACE DESTROYED namespace={namespace}");
            self.needs_redraw = true;
        }
    }
}

impl IdleInhibitHandler for GenesisState {
    fn inhibit(&mut self, surface: WlSurface) {
        self.add_inhibitor(surface);
        marker!("IDLE INHIBIT count={}", self.idle_inhibitors.len());
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.remove_inhibitor(&surface);
        marker!("IDLE UNINHIBIT count={}", self.idle_inhibitors.len());
    }
}

impl IdleNotifierHandler for GenesisState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

// The cursor-shape protocol lets a client request a shape for a
// tablet tool as well as for the pointer, so Smithay's
// `delegate_cursor_shape!` requires `TabletSeatHandler`. Genesis has
// no tablet hardware story and advertises no tablet global — this is
// the empty implementation (the default `tablet_tool_image` is a
// no-op), present only so the pointer half of cursor-shape works.
impl TabletSeatHandler for GenesisState {}

impl XdgActivationHandler for GenesisState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    /// A client asked to activate a surface (focus handoff, e.g. a
    /// notification click or `xdg-activation` token from another app).
    /// The surface is raised and given keyboard focus, exactly like a
    /// pointer press on it (see `input.rs` click-to-focus). Tokens
    /// are accepted as-is: there is no Shell focus policy yet to
    /// refuse one, and the default implementation already considers
    /// every token valid.
    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: smithay::wayland::xdg_activation::XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Stage 4: an adopted Chromium surface is never activated
        // except through its Browser window's focus. Activation
        // requests for adopted surfaces that did not come via the
        // Browser window are ignored (the manager track routes its
        // own activate through `is_adopted` the same way).
        if let Some(record) = self
            .registry
            .records()
            .iter()
            .find(|record| record.object_id == surface.id())
        {
            if self.is_adopted(record.id) {
                marker!("ACTIVATION IGNORED adopted surface={}", record.id);
                return;
            }
        }
        let window = self
            .registry
            .records()
            .iter()
            .find(|record| record.object_id == surface.id())
            .map(|record| record.window.clone());
        if let Some(window) = window {
            self.space.raise_element(&window, true);
            self.set_keyboard_focus(Some(&window));
            marker!(
                "ACTIVATION surface={}",
                self.keyboard_focus_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "(none)".to_string())
            );
        }
    }
}

delegate_foreign_toplevel_list!(GenesisState);
delegate_global_dispatch!(GenesisState: [ZwlrForeignToplevelManagerV1: ForeignToplevelManagerGlobalData] => ForeignToplevelManagerState);
delegate_dispatch!(GenesisState: [ZwlrForeignToplevelManagerV1: ()] => ForeignToplevelManagerState);
delegate_dispatch!(GenesisState: [ZwlrForeignToplevelHandleV1: u64] => ForeignToplevelManagerState);

delegate_compositor!(GenesisState);
delegate_shm!(GenesisState);
delegate_seat!(GenesisState);
delegate_output!(GenesisState);
delegate_xdg_shell!(GenesisState);
delegate_layer_shell!(GenesisState);
delegate_presentation!(GenesisState);
delegate_viewporter!(GenesisState);
delegate_content_type!(GenesisState);
delegate_single_pixel_buffer!(GenesisState);
delegate_xdg_activation!(GenesisState);
delegate_idle_inhibit!(GenesisState);
delegate_idle_notify!(GenesisState);
delegate_cursor_shape!(GenesisState);
