// SPDX-License-Identifier: GPL-3.0-or-later
//! Text clipboard — the `wl_data_device` selection (copy/paste).
//!
//! The compositor advertises `wl_data_device_manager` (Smithay's
//! `selection` module) so one client can set the selection and
//! another can read it: the copy side offers `text/plain` and serves
//! the bytes on request; the paste side receives the offer and reads
//! them. That is the whole clipboard story in v1 — the protocol
//! moves bytes between clients, and the compositor never sees them:
//! the only marker here lists MIME types, never contents, and no
//! clipboard state is ever logged or persisted.
//!
//! Two rules from the protocol shape the wiring:
//!
//! * **Only the focused client may set the selection.** Smithay
//!   enforces it in `set_selection` (the setter must hold keyboard
//!   focus). [`note_keyboard_focus`] is called from
//!   `SeatHandler::focus_changed` so the data-device focus follows
//!   keyboard focus — the selection is offered to the newly focused
//!   client, and a client that just took focus can set it.
//! * **Drag-and-drop is not this module.** `DataDeviceHandler`
//!   requires the DnD grab traits, so they are implemented with
//!   their empty defaults — client-initiated drags run Smithay's
//!   protocol mechanics but no Genesis policy. Real drag-and-drop
//!   (gap #18) is a follow-up, not a side effect of the clipboard.

use smithay::{
    delegate_data_device,
    input::Seat,
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Client, Resource},
    wayland::selection::{
        data_device::{
            set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
            ServerDndGrabHandler,
        },
        SelectionHandler, SelectionSource, SelectionTarget,
    },
};

use crate::{marker, server::GenesisState};

impl SelectionHandler for GenesisState {
    type SelectionUserData = ();

    /// A client set (or cleared) the selection. The marker names
    /// the MIME types only — the bytes themselves never pass
    /// through the compositor, and nothing here may ever log them.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        match source {
            None => marker!("CLIPBOARD SELECTION cleared"),
            Some(source) => {
                let mimes = source.mime_types();
                marker!(
                    "CLIPBOARD SELECTION mime={}",
                    if mimes.is_empty() {
                        "(none)".to_string()
                    } else {
                        mimes.join(",")
                    }
                );
            }
        }
    }
}

// Drag-and-drop grab handlers: empty defaults (see the module docs).
// The traits are required by DataDeviceHandler; no Genesis DnD
// policy exists yet.
impl ClientDndGrabHandler for GenesisState {}
impl ServerDndGrabHandler for GenesisState {}

impl DataDeviceHandler for GenesisState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

delegate_data_device!(GenesisState);

/// Route the clipboard (data-device) focus to the client owning the
/// newly keyboard-focused surface, or clear it when keyboard focus
/// is gone. Called from `SeatHandler::focus_changed`: the current
/// selection is offered to the newly focused client (if it has a
/// data device), and only that client may set a new selection.
pub fn note_keyboard_focus(
    state: &GenesisState,
    seat: &Seat<GenesisState>,
    focused: Option<&WlSurface>,
) {
    let client: Option<Client> = focused.and_then(|surface| surface.client());
    set_data_device_focus(&state.dh, seat, client);
}
