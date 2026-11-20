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

use smithay::{
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatHandler, SeatState},
    reexports::wayland_server::{
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
        Client, DisplayHandle, Resource,
    },
    utils::{Logical, Point, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{with_states, CompositorClientState, CompositorHandler, CompositorState},
        output::{OutputHandler, OutputManagerState},
        seat::WaylandFocus,
        shell::xdg::XdgShellState,
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
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

    pub seat: Seat<Self>,
    /// The seat's keyboard handle (created once; Stage 2 has one seat).
    pub keyboard: smithay::input::keyboard::KeyboardHandle<Self>,
    /// The seat's pointer handle.
    pub pointer: smithay::input::pointer::PointerHandle<Self>,

    /// The scene. Genesis owns the render list and positions each
    /// surface's element — that is the adoption mechanism (Stage 4).
    pub space: Space<Window>,
    /// The one output (Stage 2: exactly one).
    pub output: smithay::output::Output,
    /// The output size in logical pixels (== physical at scale 1).
    pub output_size: Size<i32, Logical>,

    pub popup_manager: PopupManager,
    pub registry: SurfaceRegistry,

    /// Serials handed to clients for input/focus events.
    serial_counter: u32,
    /// Last pointer location in output (logical) coordinates.
    pub pointer_location: Point<f64, Logical>,
    /// Surface id under the pointer, if any.
    pub pointer_focus_id: Option<u64>,
    /// Surface id holding keyboard focus, if any.
    pub keyboard_focus_id: Option<u64>,
    /// Set when a frame needs presenting (commit, new surface, resize…).
    pub needs_redraw: bool,
    /// Frames presented since start (for the `FIRST FRAME` marker).
    pub frames_presented: u64,
    /// Monotonic start time, for `wl_surface.frame` callbacks.
    pub start_time: std::time::Instant,

    /// Client connect/disconnect events, pushed by `ClientState`.
    pub client_events: Arc<Mutex<Vec<ClientEvent>>>,
}

impl GenesisState {
    pub fn new(dh: DisplayHandle, output_size: Size<i32, Logical>) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "sevyn");
        let keyboard = seat
            .add_keyboard(Default::default(), 200, 200)
            .expect("seat keyboard");
        let pointer = seat.add_pointer();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let output = output_mod::create_output(&dh, output_size);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        Self {
            dh: dh.clone(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            output_manager_state,
            seat,
            keyboard,
            pointer,
            space,
            output,
            output_size,
            popup_manager: PopupManager::default(),
            registry: SurfaceRegistry::new(),
            serial_counter: 1,
            pointer_location: (0.0, 0.0).into(),
            pointer_focus_id: None,
            keyboard_focus_id: None,
            needs_redraw: true,
            frames_presented: 0,
            start_time: std::time::Instant::now(),
            client_events: Arc::new(Mutex::new(Vec::new())),
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
    pub fn set_keyboard_focus(&mut self, window: Option<&Window>) {
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
        let id = focused.and_then(|surface| {
            self.registry
                .by_object_id(&surface.id())
                .map(|record| record.id)
        });
        marker!(
            "FOCUS KEYBOARD surface={}",
            id.map(|id| id.to_string())
                .unwrap_or_else(|| "(none)".to_string())
        );
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
        // Nested mode: the parent compositor draws the cursor over our
        // window. A compositor-owned cursor (hardware cursor plane, GL
        // fallback, cursor-shape protocol) is Stage 3.
    }
}

delegate_compositor!(GenesisState);
delegate_shm!(GenesisState);
delegate_seat!(GenesisState);
delegate_output!(GenesisState);
delegate_xdg_shell!(GenesisState);
