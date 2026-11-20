// SPDX-License-Identifier: GPL-3.0-or-later
//! Surface registry — the compositor's record of every toplevel surface.
//!
//! One [`SurfaceRecord`] per `xdg_toplevel`, keyed by the Wayland object id
//! of its `wl_surface`. The registry is the compositor's side of the
//! surface ↔ Runtime-session association: each record carries the
//! [`SessionId`](crate::runtime_bridge::SessionId) it was associated with,
//! the client pid the association was derived from, and the window the
//! compositor placed in the scene.
//!
//! Placement is compositor-owned (see `server.rs`): the compositor chooses
//! where every window goes in the `Space`; a client never positions itself.
//! That is the property Stage 4's foreign-toplevel adoption relies on —
//! adoption is the compositor moving a surface's element in its own scene,
//! not a client request.

use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::{
    desktop::Window,
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    utils::Rectangle,
    wayland::shell::xdg::ToplevelSurface,
};

use crate::runtime_bridge::SessionId;

/// One toplevel surface the compositor knows about.
#[derive(Debug)]
pub struct SurfaceRecord {
    /// Compositor-assigned id, stable for the surface's lifetime. Used in
    /// markers and bridge messages so the Shell can name a surface.
    pub id: u64,
    /// The Wayland object id of the toplevel's `wl_surface`.
    pub object_id: ObjectId,
    /// The compositor-placed window in the scene.
    pub window: Window,
    /// The `xdg_toplevel` itself (for `send_close`).
    pub toplevel: ToplevelSurface,
    /// Client-set app id (`xdg_toplevel.set_app_id`), empty until set.
    pub app_id: String,
    /// Client-set title, empty until set.
    pub title: String,
    /// The Runtime session this surface is associated with.
    pub session: SessionId,
    /// Last known window geometry (logical). `(0, 0)` until the first
    /// commit with a buffer.
    pub size: (i32, i32),
    /// Whether the first commit with a buffer has been seen.
    pub has_buffer: bool,
    /// Whether the Shell asked for this surface to close (bridge `close`).
    pub close_requested: bool,
}

impl SurfaceRecord {
    /// The `wl_surface` of the toplevel.
    pub fn wl_surface(&self) -> &WlSurface {
        self.toplevel.wl_surface()
    }
}

/// The registry. Single-threaded: only the main loop touches it.
#[derive(Debug, Default)]
pub struct SurfaceRegistry {
    next_id: u64,
    records: Vec<SurfaceRecord>,
}

impl SurfaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new toplevel. The caller has already associated the
    /// surface with a session (see `runtime_bridge`); the pid lives in
    /// that association, not here.
    pub fn register(
        &mut self,
        toplevel: ToplevelSurface,
        window: Window,
        app_id: String,
        title: String,
        session: SessionId,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let object_id = toplevel.wl_surface().id();
        self.records.push(SurfaceRecord {
            id,
            object_id,
            window,
            toplevel,
            app_id,
            title,
            session,
            size: (0, 0),
            has_buffer: false,
            close_requested: false,
        });
        id
    }

    pub fn get(&self, id: u64) -> Option<&SurfaceRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut SurfaceRecord> {
        self.records.iter_mut().find(|record| record.id == id)
    }

    pub fn by_object_id(&self, object_id: &ObjectId) -> Option<&SurfaceRecord> {
        self.records
            .iter()
            .find(|record| &record.object_id == object_id)
    }

    pub fn by_session(&mut self, session: &SessionId) -> Option<&mut SurfaceRecord> {
        self.records
            .iter_mut()
            .find(|record| &record.session == session)
    }

    pub fn records(&self) -> &[SurfaceRecord] {
        &self.records
    }

    /// Remove a record, returning it. The caller emits the
    /// `SESSION STOPPED` marker via `runtime_bridge::disassociate`.
    pub fn remove(&mut self, id: u64) -> Option<SurfaceRecord> {
        let index = self.records.iter().position(|record| record.id == id)?;
        Some(self.records.remove(index))
    }

    /// The record for a toplevel surface, if it is registered.
    pub fn for_toplevel(&self, toplevel: &ToplevelSurface) -> Option<&SurfaceRecord> {
        self.by_object_id(&toplevel.wl_surface().id())
    }

    /// Refresh a record's geometry from its window. Returns the new size
    /// when it changed (including the first non-zero size).
    pub fn refresh_geometry(&mut self, id: u64) -> Option<(i32, i32)> {
        let record = self.get_mut(id)?;
        let geometry: Rectangle<i32, smithay::utils::Logical> = record.window.geometry();
        let size = (geometry.size.w, geometry.size.h);
        if size == record.size {
            return None;
        }
        let first = record.size == (0, 0) && size != (0, 0);
        record.size = size;
        if size != (0, 0) {
            record.has_buffer = true;
        }
        if first || record.has_buffer {
            Some(size)
        } else {
            None
        }
    }
}
