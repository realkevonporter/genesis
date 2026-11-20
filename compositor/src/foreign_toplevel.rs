// SPDX-License-Identifier: GPL-3.0-or-later
//! `zwlr_foreign_toplevel_manager_v1` (Stage 4) + `ext-foreign-toplevel-list-v1`.
//!
//! The manager global is the one gap Smithay 0.7.0 leaves Genesis (see
//! COMPOSITOR_PLAN.md, "Language: Rust"): Smithay ships
//! `wayland::foreign_toplevel_list` (the list-only
//! `ext-foreign-toplevel-list-v1`) but no
//! `zwlr_foreign_toplevel_manager_v1`. Genesis implements the manager
//! global itself on top of `wayland-server`, using the maintained
//! generated types from `wayland-protocols-wlr`
//! (`foreign_toplevel::v1`, server module). The protocol's role is
//! listing / control / association ONLY — never the embedding:
//! adoption is compositor-internal scene placement, owned by the
//! adoption track, and no manager request can place a surface.
//!
//! What the manager offers (protocol version 2, per the plan — the
//! upstream XML declares version 3, but v3 only adds the `parent`
//! event, which Genesis does not send):
//!
//! * On bind, a `toplevel` event for every currently open toplevel,
//!   each followed by its `title`, `app_id`, `output_enter` (the one
//!   output), `state`, and `done` — and the same events as toplevels
//!   open, close, or change title / app_id / state.
//! * The handle requests, acting on the named toplevel: `activate`,
//!   `close`, `set_minimized` / `unset_minimized`, `set_maximized` /
//!   `unset_maximized`, `set_fullscreen` / `unset_fullscreen`,
//!   `set_rectangle` (a minimize hint only — it is NOT placement).
//!   `stop` ends the manager's stream (`finished` is sent back).
//! * `close` forwards to the compositor's existing close path
//!   ([`GenesisState::close_surface`]); maximize / fullscreen set
//!   the corresponding xdg-toplevel state and send a configure
//!   (maximize / fullscreen also configure the output size and move
//!   the window to (0, 0)). Minimize has NO xdg-toplevel state —
//!   xdg-shell defines none — so it is manager-side bookkeeping,
//!   reported in the `state` array: the window is unmapped from the
//!   scene (it stops rendering and receiving input) until it is
//!   unminimized, and a configure is still sent so the client knows
//!   something changed.
//!
//! What it deliberately does NOT do:
//!
//! * No adoption flag in the protocol — there is none. The manager
//!   reports every toplevel (adopted or not) with its app_id / title /
//!   state; the Shell hides adopted surfaces from the window list
//!   because the Runtime tells it the toplevel is adopted, not
//!   because the manager says so.
//! * No direct activation of an adopted surface: a manager `activate`
//!   for an adopted toplevel is IGNORED (marker `MANAGER ACTIVATE
//!   IGNORED ... adopted`). Adopted surfaces are activated only
//!   through their Browser window's focus.
//! * No placement, no move/resize requests — the protocol has none,
//!   and `set_rectangle` is stored as a minimize hint and nothing
//!   more.
//!
//! Adoption interface (the adoption track owns the real mapping; this
//! module carries the seam so the manager can be built and proven
//! before the adoption track lands):
//!
//! * [`GenesisState::is_adopted`] — backed by a set this module owns,
//!   filled by [`GenesisState::note_adopted`] / cleared by
//!   [`GenesisState::note_released`]. The adoption track calls those
//!   when it adopts / releases, or replaces the backing with its own
//!   registry at integration — the signature is the contract.
//! * [`GenesisState::adopted_browser_window`] — STUB: returns `None`
//!   until the adoption track provides the mapping. The manager never
//!   calls it (ignoring an activation needs only `is_adopted`); it
//!   exists so the interface the task names is present in one place.
//!   If the adoption track's interface differs at integration, the
//!   coordinator resolves it.
//!
//! Wiring: the state lives on [`GenesisState`] as
//! `foreign_toplevel_manager_state` (this module) and
//! `foreign_toplevel_list_state` (Smithay's list state); the delegate
//! macros live in `server.rs`. The per-loop sync
//! ([`GenesisState::sync_foreign_toplevels`]) runs at the end of
//! `reconcile_surfaces`, so announcements lag a toplevel's creation
//! by at most one main-loop iteration.

use std::collections::HashSet;

use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState,
        wayland_server::{
            backend::GlobalId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
            Resource, Weak,
        },
    },
    utils::{Logical, Point},
    wayland::{
        foreign_toplevel_list::{
            ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
        },
        shell::xdg::ToplevelSurface,
    },
};
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::{marker, server::GenesisState};

/// The advertised `zwlr_foreign_toplevel_manager_v1` version: 2, per
/// the plan ("protocol version 2 in wlroots"). The upstream XML
/// declares version 3, but v3 only adds the `parent` event, which
/// Genesis never sends, so advertising 2 is the honest version.
pub const MANAGER_VERSION: u32 = 2;

/// Global data for the manager global. There is no filter: every
/// client may bind the manager (the Shell is the intended client, and
/// the protocol is listing/control only — adoption itself is
/// privileged and goes over the Runtime socket, not this global).
#[derive(Debug)]
pub struct ForeignToplevelManagerGlobalData;

/// One manager-protocol state value, in the order the XML numbers
/// them (`wlr-foreign-toplevel-management-unstable-v1.xml`, enum
/// `state`: maximized 0, minimized 1, activated 2, fullscreen 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagerStateFlag {
    Maximized,
    Minimized,
    Activated,
    Fullscreen,
}

impl ManagerStateFlag {
    fn value(self) -> u32 {
        match self {
            ManagerStateFlag::Maximized => 0,
            ManagerStateFlag::Minimized => 1,
            ManagerStateFlag::Activated => 2,
            ManagerStateFlag::Fullscreen => 3,
        }
    }

    fn name(self) -> &'static str {
        match self {
            ManagerStateFlag::Maximized => "maximized",
            ManagerStateFlag::Minimized => "minimized",
            ManagerStateFlag::Activated => "activated",
            ManagerStateFlag::Fullscreen => "fullscreen",
        }
    }
}

/// The manager-protocol state of a toplevel: the pending
/// xdg-toplevel states (Maximized / Activated / Fullscreen) plus the
/// manager's own Minimized flag — xdg-shell has NO minimized state,
/// so minimization is manager-side bookkeeping (see
/// `manager_set_minimized`), reported here in the `state` array.
/// The state is read from the pending xdg state — the state the
/// compositor intends, which is what a manager client must see
/// immediately after it requests a change (the acked `current_state`
/// would lag one client roundtrip behind a request the manager itself
/// just made).
///
/// Reading through `with_pending_state` is safe here: when no pending
/// state exists it clones the current state into the pending slot,
/// and `has_pending_changes` compares the two, so a read creates no
/// spurious configure.
fn toplevel_manager_states(toplevel: &ToplevelSurface, minimized: bool) -> Vec<ManagerStateFlag> {
    let mut flags = Vec::new();
    toplevel.with_pending_state(|pending| {
        if pending.states.contains(ToplevelState::Maximized) {
            flags.push(ManagerStateFlag::Maximized);
        }
        if minimized {
            flags.push(ManagerStateFlag::Minimized);
        }
        if pending.states.contains(ToplevelState::Activated) {
            flags.push(ManagerStateFlag::Activated);
        }
        if pending.states.contains(ToplevelState::Fullscreen) {
            flags.push(ManagerStateFlag::Fullscreen);
        }
    });
    flags
}

/// Encode manager state flags as the `state` event's array: one
/// little-endian u32 per flag, in XML enum order.
fn encode_state(flags: &[ManagerStateFlag]) -> Vec<u8> {
    flags
        .iter()
        .flat_map(|flag| flag.value().to_le_bytes())
        .collect()
}

/// Human-readable state for markers, e.g. `activated` or
/// `activated,minimized`, or `(none)`.
fn state_names(flags: &[ManagerStateFlag]) -> String {
    if flags.is_empty() {
        "(none)".to_string()
    } else {
        flags
            .iter()
            .map(|flag| flag.name())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Decode a `state` event array back to flags (for markers). Unknown
/// values are dropped — the array only ever holds values this module
/// encoded.
fn decode_state(bytes: &[u8]) -> Vec<ManagerStateFlag> {
    let (chunks, _remainder) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .filter_map(|chunk| {
            let value = u32::from_le_bytes(*chunk);
            match value {
                0 => Some(ManagerStateFlag::Maximized),
                1 => Some(ManagerStateFlag::Minimized),
                2 => Some(ManagerStateFlag::Activated),
                3 => Some(ManagerStateFlag::Fullscreen),
                _ => None,
            }
        })
        .collect()
}

/// One toplevel as the manager + list protocols see it.
struct ManagedToplevel {
    /// The compositor-assigned surface id (see `SurfaceRegistry`).
    surface_id: u64,
    /// One handle resource per bound manager client. Weak: a client
    /// that destroys its handle (or disconnects) drops out of this
    /// list without any action from the compositor.
    handles: Vec<Weak<ZwlrForeignToplevelHandleV1>>,
    /// The Smithay list handle for this toplevel, if the list state
    /// created one (it always does for a live toplevel).
    list_handle: Option<ForeignToplevelHandle>,
    /// Last title / app_id / state array sent to manager clients.
    title: String,
    app_id: String,
    state: Vec<u8>,
    /// Whether the toplevel is minimized. xdg-shell has no minimized
    /// state, so this is manager-side bookkeeping, reported in the
    /// `state` array (see `toplevel_manager_states`).
    minimized: bool,
    /// The last `set_rectangle` minimize hint, if any. Stored only;
    /// it is NOT placement. `None` also means "removed" (a
    /// width=height=0 rectangle clears it).
    rectangle: Option<(i32, i32, i32, i32)>,
    /// Window location saved when the window was maximized or
    /// fullscreened, restored when both are unset. `None` while
    /// neither is set (or while the window was never moved for them).
    saved_location: Option<Point<i32, Logical>>,
    /// Window location saved when the window was minimized, restored
    /// on unminimize. `None` while not minimized.
    minimized_location: Option<Point<i32, Logical>>,
}

/// The manager global's state, owned by [`GenesisState`].
pub struct ForeignToplevelManagerState {
    /// Retained so the global stays advertised while the compositor
    /// runs (dropping it would unadvertise the global).
    #[allow(dead_code)]
    global: GlobalId,
    /// Bound manager resources (one per client that bound the
    /// global). A client that sends `stop` — or disconnects — is
    /// removed here, so it receives no further `toplevel` events.
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// One entry per live toplevel.
    toplevels: Vec<ManagedToplevel>,
    /// Surface ids currently adopted into a Browser window. Filled
    /// by [`GenesisState::note_adopted`]; the adoption track owns the
    /// real adoption record and calls it (or replaces this backing
    /// at integration). Until then this is the stubbed adoption
    /// state the manager's ignored-activation proof uses.
    adopted: HashSet<u64>,
}

impl ForeignToplevelManagerState {
    /// Create the manager global on the display.
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<GenesisState, ZwlrForeignToplevelManagerV1, _>(
            MANAGER_VERSION,
            ForeignToplevelManagerGlobalData,
        );
        Self {
            global,
            managers: Vec::new(),
            toplevels: Vec::new(),
            adopted: HashSet::new(),
        }
    }

    fn entry(&self, surface_id: u64) -> Option<&ManagedToplevel> {
        self.toplevels
            .iter()
            .find(|entry| entry.surface_id == surface_id)
    }

    fn entry_mut(&mut self, surface_id: u64) -> Option<&mut ManagedToplevel> {
        self.toplevels
            .iter_mut()
            .find(|entry| entry.surface_id == surface_id)
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData, GenesisState>
    for ForeignToplevelManagerState
{
    fn bind(
        state: &mut GenesisState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelManagerGlobalData,
        data_init: &mut DataInit<'_, GenesisState>,
    ) {
        state.manager_bind(resource, data_init);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, (), GenesisState> for ForeignToplevelManagerState {
    fn request(
        state: &mut GenesisState,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, GenesisState>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                state.manager_stop(resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut GenesisState,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state
            .foreign_toplevel_manager_state
            .managers
            .retain(|manager| manager != resource);
        marker!("MANAGER UNBOUND");
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, u64, GenesisState> for ForeignToplevelManagerState {
    fn request(
        state: &mut GenesisState,
        _client: &Client,
        _resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, GenesisState>,
    ) {
        let surface_id = *data;
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.manager_set_maximized(surface_id, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.manager_set_maximized(surface_id, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.manager_set_minimized(surface_id, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                state.manager_set_minimized(surface_id, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.manager_activate(surface_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.manager_close(surface_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle {
                x,
                y,
                width,
                height,
                ..
            } => {
                state.manager_set_rectangle(surface_id, x, y, width, height);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. } => {
                state.manager_set_fullscreen(surface_id, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.manager_set_fullscreen(surface_id, false);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut GenesisState,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        _data: &u64,
    ) {
        // Prune the dead handle from its entry so later event sends
        // do not keep a stale weak around. The entry itself stays
        // until the toplevel closes.
        for entry in &mut state.foreign_toplevel_manager_state.toplevels {
            entry.handles.retain(|weak| {
                weak.upgrade()
                    .map(|handle| &handle != resource)
                    .unwrap_or(false)
            });
        }
    }
}

impl ForeignToplevelListHandler for GenesisState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

impl GenesisState {
    /// Whether the toplevel is adopted into a Browser window (Stage 4
    /// adoption). Backed by the manager's adoption set — see the
    /// module docs. The adoption track fills it via
    /// [`GenesisState::note_adopted`].
    pub fn is_adopted(&self, surface_id: u64) -> bool {
        self.foreign_toplevel_manager_state
            .adopted
            .contains(&surface_id)
    }

    /// The Browser window a toplevel is adopted into, if any.
    ///
    /// STUB: returns `None` until the adoption track provides the
    /// mapping. The manager never calls it — ignoring a direct
    /// activation needs only [`GenesisState::is_adopted`] — it exists
    /// so the interface the task names is present in one place. If
    /// the adoption track's interface differs at integration, the
    /// coordinator resolves it.
    ///
    /// Unused until the adoption track lands (hence the allow).
    #[allow(dead_code)]
    pub fn adopted_browser_window(&self, _surface_id: u64) -> Option<Window> {
        None
    }

    /// Integration seam: record that a surface is adopted into a
    /// Browser window. The adoption track calls this when it adopts;
    /// [`GenesisState::note_released`] clears it.
    ///
    /// Unused until the adoption track lands (hence the allow).
    #[allow(dead_code)]
    pub fn note_adopted(&mut self, surface_id: u64) {
        self.foreign_toplevel_manager_state
            .adopted
            .insert(surface_id);
    }

    /// Integration seam: record that a surface is no longer adopted.
    ///
    /// Unused until the adoption track lands (hence the allow).
    #[allow(dead_code)]
    pub fn note_released(&mut self, surface_id: u64) {
        self.foreign_toplevel_manager_state
            .adopted
            .remove(&surface_id);
    }

    /// The manager-protocol state array for a toplevel, from its
    /// pending xdg-toplevel state plus the manager's own minimized
    /// flag (see `toplevel_manager_states`).
    fn state_array_for(toplevel: &ToplevelSurface, minimized: bool) -> Vec<u8> {
        encode_state(&toplevel_manager_states(toplevel, minimized))
    }

    /// Whether the toplevel is minimized, from its manager entry
    /// (false when there is no entry yet — e.g. before the first
    /// sync created one).
    fn is_minimized(&self, surface_id: u64) -> bool {
        self.foreign_toplevel_manager_state
            .entry(surface_id)
            .map(|entry| entry.minimized)
            .unwrap_or(false)
    }

    /// Make sure a manager entry exists for a live toplevel. A
    /// manager request can arrive in the same dispatch round as the
    /// toplevel's creation, before the sync has created the entry —
    /// a request that mutates entry state must not silently lose
    /// the mutation when the sync later creates a fresh entry.
    fn ensure_entry_for_record(&mut self, surface_id: u64) {
        let Some((app_id, title, state_bytes)) = self.registry.get(surface_id).map(|record| {
            (
                record.app_id.clone(),
                record.title.clone(),
                Self::state_array_for(&record.toplevel, false),
            )
        }) else {
            return;
        };
        self.ensure_manager_entry(surface_id, &app_id, &title, &state_bytes);
    }

    /// Live manager-handle resources for a toplevel (dead weaks
    /// dropped). Collected first so the caller can act on the
    /// compositor state and then send events without holding a
    /// borrow on the manager state.
    fn manager_handles(&self, surface_id: u64) -> Vec<ZwlrForeignToplevelHandleV1> {
        self.foreign_toplevel_manager_state
            .entry(surface_id)
            .map(|entry| {
                entry
                    .handles
                    .iter()
                    .filter_map(|weak| weak.upgrade().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Send the toplevel's current `state` + `done` to every manager
    /// client holding a handle for it, and record what was sent.
    /// Called after a manager request changes the state, and by the
    /// sync when the state changed.
    fn send_manager_state(&mut self, surface_id: u64) {
        let Some(record) = self.registry.get(surface_id) else {
            return;
        };
        let toplevel = record.toplevel.clone();
        let minimized = self.is_minimized(surface_id);
        let state_bytes = Self::state_array_for(&toplevel, minimized);
        let flags = toplevel_manager_states(&toplevel, minimized);
        let handles = self.manager_handles(surface_id);
        if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
            entry.state = state_bytes.clone();
        }
        for handle in &handles {
            handle.state(state_bytes.clone());
            handle.done();
        }
        if !handles.is_empty() {
            marker!(
                "MANAGER STATE id={surface_id} state={}",
                state_names(&flags)
            );
        }
    }

    /// A client bound the manager global: register it, then send it
    /// a `toplevel` event (with title / app_id / output_enter /
    /// state / done) for every currently open toplevel.
    fn manager_bind(
        &mut self,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        self.foreign_toplevel_manager_state
            .managers
            .push(manager.clone());
        // Snapshot the live toplevels first: ensuring entries below
        // borrows the manager state, and the snapshot must not hold
        // a registry borrow across it.
        let live: Vec<(u64, String, String, Vec<u8>)> = self
            .registry
            .records()
            .iter()
            .map(|record| {
                (
                    record.id,
                    record.app_id.clone(),
                    record.title.clone(),
                    Self::state_array_for(&record.toplevel, self.is_minimized(record.id)),
                )
            })
            .collect();
        for (id, app_id, title, state_bytes) in &live {
            let created = self.ensure_manager_entry(*id, app_id, title, state_bytes);
            if !created {
                // The entry pre-dates this manager: this manager has
                // not seen the toplevel yet, so announce it to this
                // manager alone.
                self.announce_to_manager(&manager, *id, app_id, title, state_bytes);
            }
        }
        marker!("MANAGER BOUND toplevels={}", live.len());
    }

    /// A manager client sent `stop`: drop it from the list (no
    /// further `toplevel` events) and send `finished`.
    fn manager_stop(&mut self, manager: &ZwlrForeignToplevelManagerV1) {
        self.foreign_toplevel_manager_state
            .managers
            .retain(|bound| bound != manager);
        manager.finished();
        marker!("MANAGER STOPPED");
    }

    /// Create the manager + list entry for a toplevel if it does not
    /// exist yet, announcing it to every bound manager. Returns
    /// whether the entry was created (false = it already existed).
    fn ensure_manager_entry(
        &mut self,
        surface_id: u64,
        app_id: &str,
        title: &str,
        state_bytes: &[u8],
    ) -> bool {
        if self
            .foreign_toplevel_manager_state
            .entry(surface_id)
            .is_some()
        {
            return false;
        }
        let list_handle = self
            .foreign_toplevel_list_state
            .new_toplevel::<Self>(title.to_string(), app_id.to_string());
        self.foreign_toplevel_manager_state
            .toplevels
            .push(ManagedToplevel {
                surface_id,
                handles: Vec::new(),
                list_handle: Some(list_handle),
                title: title.to_string(),
                app_id: app_id.to_string(),
                state: state_bytes.to_vec(),
                minimized: false,
                rectangle: None,
                saved_location: None,
                minimized_location: None,
            });
        self.announce_to_managers(surface_id, app_id, title, state_bytes);
        marker!("MANAGER TOPLEVEL id={surface_id} app_id={app_id} title={title}");
        true
    }

    /// Send a `toplevel` event (with title / app_id / output_enter /
    /// state / done) for one toplevel to every bound manager client,
    /// and remember the handle resources on the entry.
    fn announce_to_managers(
        &mut self,
        surface_id: u64,
        app_id: &str,
        title: &str,
        state_bytes: &[u8],
    ) {
        let managers = self.foreign_toplevel_manager_state.managers.clone();
        for manager in &managers {
            self.announce_to_manager(manager, surface_id, app_id, title, state_bytes);
        }
    }

    /// The same announcement, to one manager client only.
    fn announce_to_manager(
        &mut self,
        manager: &ZwlrForeignToplevelManagerV1,
        surface_id: u64,
        app_id: &str,
        title: &str,
        state_bytes: &[u8],
    ) {
        let Some(client) = manager.client() else {
            return;
        };
        let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, Self>(
            &self.dh,
            manager.version(),
            surface_id,
        ) else {
            return;
        };
        manager.toplevel(&handle);
        handle.title(title.to_string());
        handle.app_id(app_id.to_string());
        // Single output: every toplevel is visible on it. The
        // `wl_output` resource is per-client, from the output
        // itself (`Output::client_outputs`).
        if let Some(output) = self.output.client_outputs(&client).next() {
            handle.output_enter(&output);
        }
        handle.state(state_bytes.to_vec());
        handle.done();
        if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
            entry.handles.push(handle.downgrade());
        }
    }

    /// The toplevel is gone: send `closed` to every manager client
    /// holding a handle for it, close the list handle, and drop the
    /// entry (plus any adoption record for it).
    fn close_manager_entry(&mut self, surface_id: u64) {
        let Some(entry) = self
            .foreign_toplevel_manager_state
            .toplevels
            .iter()
            .position(|entry| entry.surface_id == surface_id)
            .map(|index| self.foreign_toplevel_manager_state.toplevels.remove(index))
        else {
            return;
        };
        for weak in &entry.handles {
            if let Ok(handle) = weak.upgrade() {
                handle.closed();
            }
        }
        if let Some(list_handle) = entry.list_handle {
            list_handle.send_closed();
        }
        self.foreign_toplevel_manager_state
            .adopted
            .remove(&surface_id);
        marker!("MANAGER TOPLEVEL CLOSED id={surface_id}");
    }

    /// Per-loop sync of the manager + list protocols against the
    /// registry. Called at the end of `reconcile_surfaces`, so an
    /// announcement lags a toplevel's creation by at most one
    /// main-loop iteration:
    ///
    /// * entries for new toplevels are created (list handle +
    ///   announcement to every bound manager);
    /// * title / app_id / state changes are sent to every manager
    ///   client holding a handle (`title` / `app_id` / `state` +
    ///   `done`) and to the list handle;
    /// * entries for toplevels that are gone are closed.
    pub fn sync_foreign_toplevels(&mut self) {
        let live: Vec<(u64, String, String, Vec<u8>)> = self
            .registry
            .records()
            .iter()
            .map(|record| {
                (
                    record.id,
                    record.app_id.clone(),
                    record.title.clone(),
                    Self::state_array_for(&record.toplevel, self.is_minimized(record.id)),
                )
            })
            .collect();
        let live_ids: HashSet<u64> = live.iter().map(|(id, _, _, _)| *id).collect();
        let gone: Vec<u64> = self
            .foreign_toplevel_manager_state
            .toplevels
            .iter()
            .map(|entry| entry.surface_id)
            .filter(|id| !live_ids.contains(id))
            .collect();
        for id in gone {
            self.close_manager_entry(id);
        }
        for (id, app_id, title, state_bytes) in &live {
            let created = self.ensure_manager_entry(*id, app_id, title, state_bytes);
            if created {
                continue;
            }
            // The entry exists: send whatever changed.
            let (title_changed, app_id_changed, state_changed) = {
                let Some(entry) = self.foreign_toplevel_manager_state.entry(*id) else {
                    continue;
                };
                (
                    entry.title != *title,
                    entry.app_id != *app_id,
                    entry.state != *state_bytes,
                )
            };
            if !title_changed && !app_id_changed && !state_changed {
                continue;
            }
            let handles = self.manager_handles(*id);
            let list_handle = self
                .foreign_toplevel_manager_state
                .entry(*id)
                .and_then(|entry| entry.list_handle.clone());
            if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(*id) {
                entry.title = title.clone();
                entry.app_id = app_id.clone();
                entry.state = state_bytes.clone();
            }
            for handle in &handles {
                if title_changed {
                    handle.title(title.clone());
                }
                if app_id_changed {
                    handle.app_id(app_id.clone());
                }
                if state_changed {
                    handle.state(state_bytes.clone());
                }
                handle.done();
            }
            if let Some(list_handle) = list_handle {
                if title_changed {
                    list_handle.send_title(title);
                }
                if app_id_changed {
                    list_handle.send_app_id(app_id);
                }
                if title_changed || app_id_changed {
                    list_handle.send_done();
                }
            }
            if !handles.is_empty() {
                marker!(
                    "MANAGER TOPLEVEL UPDATE id={id} app_id={app_id} title={title} state={}",
                    state_names(&decode_state(state_bytes))
                );
            }
        }
    }

    /// Manager `activate`: raise the window and give it keyboard
    /// focus — unless the toplevel is adopted, in which case the
    /// request is IGNORED: adopted surfaces are activated only
    /// through their Browser window's focus, never directly.
    /// Activating a minimized window restores it first: a minimized
    /// window cannot be the visible focused window.
    pub fn manager_activate(&mut self, surface_id: u64) {
        if self.is_adopted(surface_id) {
            marker!("MANAGER ACTIVATE IGNORED id={surface_id} adopted");
            return;
        }
        let Some(window) = self
            .registry
            .get(surface_id)
            .map(|record| record.window.clone())
        else {
            marker!("MANAGER ACTIVATE UNKNOWN id={surface_id}");
            return;
        };
        if self.is_minimized(surface_id) {
            self.manager_set_minimized(surface_id, false);
        }
        self.space.raise_element(&window, true);
        self.set_keyboard_focus(Some(&window));
        marker!("MANAGER ACTIVATE id={surface_id}");
    }

    /// Manager `close`: forward to the compositor's existing close
    /// path (the client is asked to close; when it goes away the
    /// sync sends `closed`).
    pub fn manager_close(&mut self, surface_id: u64) {
        let ok = self.close_surface(surface_id);
        marker!("MANAGER CLOSE id={surface_id} ok={ok}");
    }

    /// Manager `set_minimized` / `unset_minimized`. xdg-shell has
    /// NO minimized state, so minimization is manager-side
    /// bookkeeping: the flag is recorded on the entry and reported
    /// in the manager `state` array, and a configure is still sent
    /// so the client knows something changed. Minimizing also
    /// unmaps the window from the scene (it stops rendering and
    /// stops receiving input) and moves keyboard focus to the
    /// topmost remaining window when it had focus; unminimizing
    /// re-maps it at its saved location.
    pub fn manager_set_minimized(&mut self, surface_id: u64, minimized: bool) {
        self.ensure_entry_for_record(surface_id);
        let Some((window, toplevel)) = self
            .registry
            .get(surface_id)
            .map(|record| (record.window.clone(), record.toplevel.clone()))
        else {
            marker!("MANAGER MINIMIZE UNKNOWN id={surface_id}");
            return;
        };
        let already = self.is_minimized(surface_id);
        if already == minimized {
            // Already in the requested state: still re-send the
            // configure + state echo so the manager client sees a
            // response to its request.
            toplevel.send_configure();
            self.send_manager_state(surface_id);
            marker!("MANAGER MINIMIZE id={surface_id} minimized={minimized} (unchanged)");
            return;
        }
        if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
            entry.minimized = minimized;
        }
        toplevel.send_configure();
        if minimized {
            let location = self.space.element_location(&window);
            if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
                entry.minimized_location = location;
            }
            self.space.unmap_elem(&window);
            if self.keyboard_focus_id == Some(surface_id) {
                self.keyboard_focus_id = None;
                let topmost = self.space.elements().next_back().cloned();
                self.set_keyboard_focus(topmost.as_ref());
            }
        } else {
            let location = self
                .foreign_toplevel_manager_state
                .entry_mut(surface_id)
                .and_then(|entry| entry.minimized_location.take());
            if let Some(location) = location {
                self.space.map_element(window, location, false);
            }
        }
        self.send_manager_state(surface_id);
        marker!("MANAGER MINIMIZE id={surface_id} minimized={minimized}");
    }

    /// Manager `set_maximized` / `unset_maximized`: set the
    /// xdg-toplevel Maximized state and send a configure (with the
    /// output size while maximized, so the client resizes to fill
    /// it). Maximizing moves the window to (0, 0), saving its
    /// location; unmaximizing restores it once neither Maximized nor
    /// Fullscreen remains. Maximizing a minimized window restores
    /// it first — a maximized window is visible by definition, so
    /// the two states are contradictory.
    pub fn manager_set_maximized(&mut self, surface_id: u64, maximized: bool) {
        self.ensure_entry_for_record(surface_id);
        if maximized && self.is_minimized(surface_id) {
            self.manager_set_minimized(surface_id, false);
        }
        let Some((window, toplevel)) = self
            .registry
            .get(surface_id)
            .map(|record| (record.window.clone(), record.toplevel.clone()))
        else {
            marker!("MANAGER MAXIMIZE UNKNOWN id={surface_id}");
            return;
        };
        let already = toplevel
            .with_pending_state(|pending| pending.states.contains(ToplevelState::Maximized));
        if already == maximized {
            toplevel.send_configure();
            self.send_manager_state(surface_id);
            marker!("MANAGER MAXIMIZE id={surface_id} maximized={maximized} (unchanged)");
            return;
        }
        toplevel.with_pending_state(|pending| {
            if maximized {
                pending.states.set(ToplevelState::Maximized);
                pending.size = Some(self.output_size);
            } else {
                pending.states.unset(ToplevelState::Maximized);
                pending.size = None;
            }
        });
        toplevel.send_configure();
        self.move_for_fullscreen_state(surface_id, &window, maximized);
        self.send_manager_state(surface_id);
        marker!("MANAGER MAXIMIZE id={surface_id} maximized={maximized}");
    }

    /// Manager `set_fullscreen` / `unset_fullscreen`: set the
    /// xdg-toplevel Fullscreen state and send a configure (with the
    /// output size while fullscreen). The request's output is only a
    /// hint and is ignored — Genesis has exactly one output. The
    /// window moves to (0, 0) while fullscreen, like maximize.
    /// Entering fullscreen on a minimized window restores it first,
    /// like maximize.
    pub fn manager_set_fullscreen(&mut self, surface_id: u64, fullscreen: bool) {
        self.ensure_entry_for_record(surface_id);
        if fullscreen && self.is_minimized(surface_id) {
            self.manager_set_minimized(surface_id, false);
        }
        let Some((window, toplevel)) = self
            .registry
            .get(surface_id)
            .map(|record| (record.window.clone(), record.toplevel.clone()))
        else {
            marker!("MANAGER FULLSCREEN UNKNOWN id={surface_id}");
            return;
        };
        let already = toplevel
            .with_pending_state(|pending| pending.states.contains(ToplevelState::Fullscreen));
        if already == fullscreen {
            toplevel.send_configure();
            self.send_manager_state(surface_id);
            marker!("MANAGER FULLSCREEN id={surface_id} fullscreen={fullscreen} (unchanged)");
            return;
        }
        toplevel.with_pending_state(|pending| {
            if fullscreen {
                pending.states.set(ToplevelState::Fullscreen);
                pending.size = Some(self.output_size);
            } else {
                pending.states.unset(ToplevelState::Fullscreen);
                pending.size = None;
            }
        });
        toplevel.send_configure();
        self.move_for_fullscreen_state(surface_id, &window, fullscreen);
        self.send_manager_state(surface_id);
        marker!("MANAGER FULLSCREEN id={surface_id} fullscreen={fullscreen}");
    }

    /// Shared maximize/fullscreen placement: on entering either
    /// state, save the window's location (once) and move it to
    /// (0, 0); on leaving one, restore the saved location only when
    /// neither Maximized nor Fullscreen remains.
    fn move_for_fullscreen_state(&mut self, surface_id: u64, window: &Window, entering: bool) {
        if entering {
            let location = self.space.element_location(window);
            if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
                if entry.saved_location.is_none() {
                    entry.saved_location = location;
                }
            }
            self.space.map_element(window.clone(), (0, 0), false);
        } else {
            let (maximized, fullscreen) = self
                .registry
                .get(surface_id)
                .map(|record| {
                    record.toplevel.with_pending_state(|pending| {
                        (
                            pending.states.contains(ToplevelState::Maximized),
                            pending.states.contains(ToplevelState::Fullscreen),
                        )
                    })
                })
                .unwrap_or((false, false));
            // A minimized window stays unmapped: leaving
            // maximized/fullscreen must not make it visible again
            // while it is still flagged minimized. The saved
            // location is left in place for the later restore.
            if !maximized && !fullscreen && !self.is_minimized(surface_id) {
                let location = self
                    .foreign_toplevel_manager_state
                    .entry_mut(surface_id)
                    .and_then(|entry| entry.saved_location.take());
                if let Some(location) = location {
                    self.space.map_element(window.clone(), location, false);
                }
            }
        }
        self.needs_redraw = true;
    }

    /// Manager `set_rectangle`: store the minimize hint. It is NOT
    /// placement — nothing moves. A width=height=0 rectangle clears
    /// the stored hint (per the protocol). A negative width or height
    /// is invalid: the request is ignored (the protocol's
    /// `invalid_rectangle` error is not posted — see README).
    pub fn manager_set_rectangle(
        &mut self,
        surface_id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        self.ensure_entry_for_record(surface_id);
        if width < 0 || height < 0 {
            marker!("MANAGER SET_RECTANGLE INVALID id={surface_id} width={width} height={height}");
            return;
        }
        if let Some(entry) = self.foreign_toplevel_manager_state.entry_mut(surface_id) {
            entry.rectangle = if width == 0 && height == 0 {
                None
            } else {
                Some((x, y, width, height))
            };
        }
        marker!("MANAGER SET_RECTANGLE id={surface_id} x={x} y={y} width={width} height={height}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;

    /// The `state` event array is one little-endian u32 per flag, in
    /// the XML enum's order (maximized 0, minimized 1, activated 2,
    /// fullscreen 3).
    #[test]
    fn state_array_encodes_xml_values() {
        let flags = vec![
            ManagerStateFlag::Maximized,
            ManagerStateFlag::Activated,
            ManagerStateFlag::Fullscreen,
        ];
        assert_eq!(
            encode_state(&flags),
            [0u32, 2, 3]
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<u8>>()
        );
        assert_eq!(state_names(&flags), "maximized,activated,fullscreen");
        assert_eq!(encode_state(&[]), Vec::<u8>::new());
        assert_eq!(state_names(&[]), "(none)");
        assert_eq!(decode_state(&encode_state(&flags)), flags);
        assert_eq!(decode_state(&[9, 0, 0, 0]), Vec::<ManagerStateFlag>::new());
    }

    /// The adoption set behind `is_adopted`: noting a surface
    /// adopted makes a direct manager activation ignorable, and
    /// releasing it (or the toplevel closing) clears it. This is
    /// the manager side of the ignored-activation proof, with the
    /// adoption state stubbed — the integrated proof (a real adopted
    /// surface, via the adoption track) happens at integration.
    #[test]
    fn adopted_set_tracks_note_and_release() {
        let display = Display::<GenesisState>::new().expect("headless display");
        let dh = display.handle();
        let mut manager_state = ForeignToplevelManagerState::new(&dh);
        assert!(!manager_state.adopted.contains(&7));
        manager_state.adopted.insert(7);
        assert!(manager_state.adopted.contains(&7));
        manager_state.adopted.remove(&7);
        assert!(!manager_state.adopted.contains(&7));
    }
}
