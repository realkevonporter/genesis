// SPDX-License-Identifier: GPL-3.0-or-later
//! `xdg-shell` handling — toplevel lifecycle and popups.
//!
//! A new toplevel is: wrapped in a [`Window`](smithay::desktop::Window),
//! placed by the compositor (see [`GenesisState::place_window`]), registered
//! in the surface registry, associated with a Runtime session (see
//! `runtime_bridge`), and given keyboard focus. The compositor — never the
//! client — decides where the window goes and whether it is activated.
//!
//! Popups are tracked in the `PopupManager` and rendered above their
//! parent toplevel (see `backend.rs`). A popup grab is honoured through
//! the same manager. This is the minimal correct xdg-shell surface: no
//! popup is ever silently dropped.

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
use smithay::{
    desktop::{PopupKind, Window},
    reexports::wayland_server::{protocol::wl_seat::WlSeat, Resource},
    utils::Serial,
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    marker,
    runtime_bridge::{self, SessionAssociation, SessionId},
    server::GenesisState,
};

impl XdgShellHandler for GenesisState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());

        // The client pid, from the Wayland connection credentials, is the
        // association key until the Runtime passes real session ids.
        let pid = surface
            .wl_surface()
            .client()
            .and_then(|client| client.get_credentials(&self.dh).ok())
            .map(|credentials| credentials.pid as u32);

        // The surface starts activated: a new toplevel takes keyboard
        // focus (the Stage 2 default focus policy, see `input.rs`).
        surface.with_pending_state(|state| {
            state.states.set(ToplevelState::Activated);
        });
        surface.send_configure();

        self.place_window(&window);
        let session = pid
            .map(SessionId::for_client_pid)
            .unwrap_or_else(|| SessionId::unknown(self.registry.records().len() as u64 + 1));
        let id = self.registry.register(
            surface.clone(),
            window.clone(),
            String::new(),
            String::new(),
            session.clone(),
        );
        marker!(
            "SURFACE CREATED id={id} session={session} pid={}",
            pid.map(|pid| pid.to_string())
                .unwrap_or_else(|| "(unknown)".to_string())
        );
        runtime_bridge::associate(&SessionAssociation {
            session,
            surface_id: id,
            app_id: String::new(),
            pid,
        });
        self.set_keyboard_focus(Some(&window));
        self.needs_redraw = true;
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The registry reconciliation in the main loop emits the
        // `SESSION STOPPED` marker and unmaps the window; here we only
        // make sure the window leaves the scene immediately.
        if let Some(record) = self.registry.for_toplevel(&surface) {
            let window = record.window.clone();
            self.space.unmap_elem(&window);
        }
        self.needs_redraw = true;
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()))
        {
            marker!("POPUP TRACK FAILED error={err:?}");
            return;
        }
        if let Err(err) = surface.send_configure() {
            marker!("POPUP CONFIGURE FAILED error={err:?}");
        }
        self.needs_redraw = true;
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.popup_manager.cleanup();
        self.needs_redraw = true;
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, serial: Serial) {
        // Stage 2 has exactly one seat, so the grab always targets it.
        // The grab root is the popup's parent toplevel surface.
        let Some(root) = surface.get_parent_surface() else {
            return;
        };
        let seat = self.seat.clone();
        if let Err(err) =
            self.popup_manager
                .grab_popup(root, PopupKind::Xdg(surface), &seat, serial)
        {
            marker!("POPUP GRAB FAILED error={err:?}");
        }
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
        // Stage 2 has no repositioning policy: popups stay where the
        // compositor first placed them. A reposition request is
        // acknowledged implicitly by not sending a new configure — the
        // client keeps the last configure it received.
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.note_metadata_changed(&surface);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.note_metadata_changed(&surface);
    }
}

impl GenesisState {
    /// A toplevel's app id or title changed after creation. Refresh the
    /// registry record (the main loop's reconciliation also does this; the
    /// handler path makes the update immediate).
    fn note_metadata_changed(&mut self, surface: &ToplevelSurface) {
        let Some(record) = self.registry.for_toplevel(surface) else {
            return;
        };
        let id = record.id;
        let wl_surface = surface.wl_surface().clone();
        let (app_id, title) = with_states(&wl_surface, |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return (String::new(), String::new());
            };
            let attributes = data.lock().unwrap();
            (
                attributes.app_id.clone().unwrap_or_default(),
                attributes.title.clone().unwrap_or_default(),
            )
        });
        let Some(record) = self.registry.get_mut(id) else {
            return;
        };
        let changed = record.app_id != app_id || record.title != title;
        record.app_id = app_id;
        record.title = title;
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
