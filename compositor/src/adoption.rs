// SPDX-License-Identifier: GPL-3.0-or-later
//! Adoption — the Stage 4 compositor-internal scene placement.
//!
//! Adoption is **not** a protocol feature (see COMPOSITOR_PLAN.md, "The
//! foreign-toplevel fact"): Genesis owns its render list and places any
//! surface's element anywhere in the scene. The Runtime asks, over the
//! bridge socket, for a Chromium toplevel to be adopted into a Browser
//! window's content rect; this module is the record and the logic.
//!
//! ## The record
//!
//! One [`Adoption`] per Browser window: the Browser session id (the
//! string the Runtime passes in the `adopt` op — the same session id
//! the bridge `close` op already addresses), the Chromium pid, the
//! Chromium surface id, and the content rect in **window-local**
//! coordinates (origin (0, 152), 800x348 under the Browser's 152 px of
//! chrome, in the plan's example).
//!
//! ## Identification
//!
//! By **pid, not app_id**: the Chromium toplevel is matched by the pid
//! from its Wayland connection credentials
//! (`wl_client_get_credentials`, read here through Smithay's
//! `Client::get_credentials`). The toplevel's app_id (`chromium`) is a
//! sanity check only — a mismatch is logged, never trusted, and never
//! blocks adoption on its own: the pid is authoritative. A client
//! claiming app_id `chromium` whose pid is not the Runtime-declared
//! one is never adopted, because the pid match fails first.
//!
//! ## Placement, clipping, move/resize
//!
//! The adopted Chromium window is **unmapped from the `Space`** on
//! adopt, so the scene never renders it at its cascaded position. Both
//! backends render it explicitly (see `render::adopted_elements`) at
//! the Browser window's output position plus the rect origin, wrapped
//! in Smithay's [`CropRenderElement`] clipped to the rect — Chromium
//! pixels can never paint outside the content area, in either backend.
//! "At the rect origin" means the window's GEOMETRY origin (see
//! [`GenesisState::adopted_buffer_origin`]): the surface tree is
//! positioned by its buffer origin, so the geometry offset is
//! subtracted first — otherwise a client with an inset geometry
//! (real Chromium) renders shifted inside the rect.
//! The placement is computed at render time from the Browser window's
//! live position, so a Browser move repositions the adopted element
//! by the same delta in the same frame: the pairing is structural,
//! not two windows that happen to sit near each other (the fix for
//! M13's drag drift). A resize sends `xdg_toplevel.configure` to
//! Chromium with the new content size — a compositor configure, never
//! a CDP call.
//!
//! ## Focus
//!
//! The adopted toplevel is never activated as a standalone window.
//! Clicking the content rect focuses the Chromium surface for keys
//! (see `input.rs`, which routes through [`GenesisState::focus_adopted`]
//! here) while the `Activated` state stays on the Browser toplevel;
//! clicking the chrome focuses the Browser app. Activation requests
//! for adopted surfaces that did not come via the Browser window —
//! including `xdg-activation` — are ignored (see `server.rs`
//! `request_activation`).
//!
//! ## Lifecycle
//!
//! * Chromium dies: the adoption is dropped, the Runtime is notified
//!   over the bridge socket with the `chromium_died` event, and the
//!   Browser window stays open (see [`GenesisState::reconcile_adoptions`]).
//! * Browser closes: the surface vanishes, the record is dropped with
//!   it, and the adopted Chromium is asked to close too (a close is
//!   not a release — see [`GenesisState::reconcile_adoptions`]).
//! * Runtime dies: the bridge server marks the state degraded (see
//!   `runtime_bridge.rs`); new adoptions are refused until the
//!   Runtime reconnects (the connection itself is the re-registration:
//!   there is no separate register op in the v1 protocol — see the
//!   report / README for this plan ambiguity).
//! * `release`: the adoption record is dropped and the Chromium
//!   window returns to ordinary compositor placement (cascade), as a
//!   standalone window again.
//!
//! [`CropRenderElement`]: smithay::backend::renderer::element::utils::CropRenderElement

use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState,
        wayland_server::Resource,
    },
    utils::{Logical, Point, Rectangle},
    wayland::seat::WaylandFocus,
};

use crate::{marker, runtime_bridge::SessionId, server::GenesisState};

/// A content rect in Browser-window-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl ContentRect {
    /// True when the rect lies entirely inside a window of the given
    /// size, with a positive area. A rect with a zero or negative
    /// dimension is malformed, not merely outside.
    pub fn fits_in(&self, size: (i32, i32)) -> bool {
        self.x >= 0
            && self.y >= 0
            && self.w > 0
            && self.h > 0
            && self.x + self.w <= size.0
            && self.y + self.h <= size.1
    }

    /// The rect in global compositor (logical) coordinates, given the
    /// Browser window's output position.
    pub fn global(&self, origin: Point<i32, Logical>) -> Rectangle<i32, Logical> {
        Rectangle::new(
            Point::from((origin.x + self.x, origin.y + self.y)),
            smithay::utils::Size::from((self.w, self.h)),
        )
    }
}

/// One adopted Chromium surface inside one Browser window.
#[derive(Debug, Clone)]
pub struct Adoption {
    /// The Browser session id, as passed by the Runtime in `adopt`.
    pub browser_session: String,
    /// The Browser toplevel's compositor surface id.
    pub browser_surface_id: u64,
    /// The adopted Chromium toplevel's compositor surface id.
    pub chromium_surface_id: u64,
    /// The Chromium pid declared by the Runtime.
    pub chromium_pid: u32,
    /// The content rect, in Browser-window-local coordinates.
    pub rect: ContentRect,
}

/// Adoption failures. The `&'static str` is the exact `error` string
/// sent on the bridge socket (see `runtime_bridge.rs`).
pub type AdoptionError = &'static str;

impl GenesisState {
    /// The pid of the Wayland client that owns a surface, from its
    /// connection credentials. `None` when the surface has no client
    /// or the credentials cannot be read.
    pub fn client_pid_for_surface(&self, surface_id: u64) -> Option<u32> {
        let record = self.registry.get(surface_id)?;
        record
            .toplevel
            .wl_surface()
            .client()
            .and_then(|client| client.get_credentials(&self.dh).ok())
            .map(|credentials| credentials.pid as u32)
    }

    /// True when the surface is an adopted Chromium surface.
    pub fn is_adopted(&self, surface_id: u64) -> bool {
        self.adoptions
            .iter()
            .any(|adoption| adoption.chromium_surface_id == surface_id)
    }

    /// The Browser window an adopted Chromium surface belongs to, if
    /// the surface is adopted. (The manager track routes its
    /// `activate` through this check: an adopted surface is never
    /// activated except via its Browser window.) Required interface
    /// for that track — unused until it lands, hence the allow.
    #[allow(dead_code)]
    pub fn adopted_browser_window(&self, surface_id: u64) -> Option<Window> {
        let adoption = self
            .adoptions
            .iter()
            .find(|adoption| adoption.chromium_surface_id == surface_id)?;
        self.registry
            .get(adoption.browser_surface_id)
            .map(|record| record.window.clone())
    }

    /// The adoption for a Browser session, if any.
    pub fn adoption_for_session(&self, session: &str) -> Option<&Adoption> {
        self.adoptions
            .iter()
            .find(|adoption| adoption.browser_session == session)
    }

    /// The adoption whose Chromium window is `window`, if any.
    pub fn adoption_for_chromium_window(&self, window: &Window) -> Option<Adoption> {
        let id = self
            .registry
            .records()
            .iter()
            .find(|record| record.window == *window)
            .map(|record| record.id)?;
        self.adoptions
            .iter()
            .find(|adoption| adoption.chromium_surface_id == id)
            .cloned()
    }

    /// The adoption whose Browser window is `window`, if any — the
    /// mirror of `adoption_for_chromium_window`, for hit-testing: a
    /// point inside the Browser window is Browser content only when
    /// the Browser owns the adoption; any other window stacked above
    /// the Browser wins the hit even inside the adopted rect.
    pub fn adoption_for_browser_window(&self, window: &Window) -> Option<Adoption> {
        let id = self
            .registry
            .records()
            .iter()
            .find(|record| record.window == *window)
            .map(|record| record.id)?;
        self.adoptions
            .iter()
            .find(|adoption| adoption.browser_surface_id == id)
            .cloned()
    }

    /// The adopted element's position in global compositor (logical)
    /// coordinates: the Browser window's output position plus the
    /// rect origin. `None` when the Browser window is not mapped.
    /// Computed at render time from the Browser window's live
    /// position, so a Browser move moves the adopted element in the
    /// same frame — no separate move step exists to drift.
    ///
    /// This is the GEOMETRY origin: the position the window's
    /// `set_window_geometry` rect lands at. The surface tree itself
    /// is positioned by its buffer origin — see
    /// [`Self::adopted_buffer_origin`].
    pub fn adopted_location(&self, adoption: &Adoption) -> Option<Point<i32, Logical>> {
        let browser = self.registry.get(adoption.browser_surface_id)?;
        let location = self.space.element_location(&browser.window)?;
        Some(Point::from((
            location.x + adoption.rect.x,
            location.y + adoption.rect.y,
        )))
    }

    /// The adopted Chromium surface's BUFFER origin in global
    /// compositor (logical) coordinates: the geometry origin
    /// ([`Self::adopted_location`]) minus the window's geometry
    /// offset within its buffer.
    ///
    /// `render_elements_from_surface_tree` positions a surface tree
    /// by its buffer origin, while the Space renders a window's
    /// buffer at `location - geometry.loc` — the geometry origin is
    /// the position, the buffer hangs off it by the geometry offset.
    /// The two must agree, or the adopted window renders shifted by
    /// that offset inside the rect. A client whose geometry is inset
    /// within its buffer exposes the difference: real Chromium sets
    /// a window geometry inset by its shadow margins
    /// (geometry.loc = (16,10)), so without this subtraction the
    /// adopted window lands 16 px right and 10 px down from the rect
    /// origin, and pointer routing (which already treats
    /// `adopted_location` as the geometry origin — see `input.rs`
    /// `hit_test`) disagrees with the rendered pixels by the same
    /// offset. The test-client stand-in has geometry.loc (0,0), so
    /// the adoption check never caught it; the Stage 5
    /// real-Chromium check did (see
    /// `compositor/tests/real-chromium-adoption-check.sh`, R1).
    pub fn adopted_buffer_origin(&self, adoption: &Adoption) -> Option<Point<i32, Logical>> {
        let location = self.adopted_location(adoption)?;
        let offset = self
            .registry
            .get(adoption.chromium_surface_id)
            .map(|record| record.window.geometry().loc)
            .unwrap_or_default();
        Some(Point::from((location.x - offset.x, location.y - offset.y)))
    }

    /// The adopted element's clip rect in global compositor (logical)
    /// coordinates. `None` when the Browser window is not mapped.
    pub fn adopted_global_rect(&self, adoption: &Adoption) -> Option<Rectangle<i32, Logical>> {
        let browser = self.registry.get(adoption.browser_surface_id)?;
        let location = self.space.element_location(&browser.window)?;
        Some(adoption.rect.global(location))
    }

    /// Adopt a Chromium toplevel into a Browser window's content rect.
    ///
    /// The Browser window is found by session id (the same session
    /// the bridge `close` op addresses); the Chromium toplevel is
    /// found by pid. Errors, in check order:
    ///
    /// * `"runtime disconnected"` — the Runtime died; no new
    ///   adoptions until it reconnects.
    ///   NOTE: this branch is defense-in-depth and is UNREACHABLE
    ///   through the bridge in the normal shape: the bridge clears
    ///   `runtime_degraded` at accept on ANY new connection (a
    ///   connection IS the Runtime's re-registration — v1 has no
    ///   separate register op), so a degraded compositor never serves
    ///   a request on a fresh connection. The refusal is enforced at
    ///   accept time instead: with `--runtime-pid` set, a connection
    ///   from any other pid is refused (EOF) while degraded, and the
    ///   Runtime's own reconnect clears degraded. Without
    ///   `--runtime-pid` (the dev default) any same-uid connection
    ///   counts as the Runtime reconnecting. A15 asserts the
    ///   accept-time semantics, not this branch.
    /// * `"unknown session"` — no live Browser window for the session.
    /// * `"already adopted"` — the session already has an adoption, or
    ///   the Chromium surface is already adopted into another window.
    /// * `"dead chromium pid"` — no live toplevel for the pid, or the
    ///   pid is not alive.
    /// * `"rect outside browser window"` — the rect does not fit
    ///   inside the Browser window's current size.
    pub fn adopt(
        &mut self,
        session: &str,
        chromium_pid: u32,
        rect: ContentRect,
    ) -> Result<(), AdoptionError> {
        if self.runtime_degraded {
            return Err("runtime disconnected");
        }
        if self.adoption_for_session(session).is_some() {
            return Err("already adopted");
        }
        let Some(browser) = self
            .registry
            .by_session(&SessionId(session.to_string()))
            .map(|record| {
                (
                    record.id,
                    record.window.clone(),
                    record.window.geometry().size,
                )
            })
        else {
            return Err("unknown session");
        };
        let (browser_id, browser_window, browser_size) = browser;
        if !rect.fits_in((browser_size.w, browser_size.h)) {
            return Err("rect outside browser window");
        }
        // The Chromium toplevel, matched by pid — never by app_id.
        let chromium = self.registry.records().iter().find_map(|record| {
            let pid = self.client_pid_for_surface(record.id)?;
            if pid == chromium_pid {
                Some((record.id, record.window.clone(), record.app_id.clone()))
            } else {
                None
            }
        });
        let Some((chromium_id, chromium_window, chromium_app_id)) = chromium else {
            return Err("dead chromium pid");
        };
        if self.is_adopted(chromium_id) {
            return Err("already adopted");
        }
        // The Chromium surface must be a different surface from the
        // Browser window itself: adopting a window into its own
        // content rect is malformed, not an adoption.
        if chromium_id == browser_id {
            return Err("malformed request");
        }
        if chromium_pid == 0 || unsafe { libc::kill(chromium_pid as libc::pid_t, 0) } != 0 {
            return Err("dead chromium pid");
        }
        // app_id is a sanity check only: log a mismatch, do not block.
        // The pid match above is authoritative; a client claiming
        // app_id `chromium` with the wrong pid never reaches this
        // point, because no toplevel matches the declared pid.
        if !chromium_app_id.is_empty() && chromium_app_id != "chromium" {
            crate::diag!(
                "adopt: chromium surface={chromium_id} app_id={chromium_app_id:?} is not \"chromium\" (pid match is authoritative; adopting anyway)"
            );
        }
        self.adoptions.push(Adoption {
            browser_session: session.to_string(),
            browser_surface_id: browser_id,
            chromium_surface_id: chromium_id,
            chromium_pid,
            rect,
        });
        // The adopted window leaves the Space: the scene never renders
        // it at its cascaded position again. Both backends render it
        // explicitly from the adoption record (see `render.rs`).
        self.space.unmap_elem(&chromium_window);
        // Activation belongs to the Browser window now: the adopted
        // toplevel is never activated as a standalone window.
        self.set_keyboard_focus(Some(&browser_window));
        self.needs_redraw = true;
        marker!(
            "ADOPTION ADOPTED session={session} browser={browser_id} chromium={chromium_id} pid={chromium_pid} rect={},{},{},{}",
            rect.x,
            rect.y,
            rect.w,
            rect.h
        );
        Ok(())
    }

    /// Move an adoption: reposition the adopted element by updating
    /// the rect. The new position takes effect in the next render,
    /// which is the same frame the Browser window's own move renders
    /// in — there is no configure and no second step to drift.
    pub fn move_adoption(&mut self, session: &str, rect: ContentRect) -> Result<(), AdoptionError> {
        let Some(browser_size) = self
            .adoption_for_session(session)
            .and_then(|adoption| self.registry.get(adoption.browser_surface_id))
            .map(|record| record.window.geometry().size)
        else {
            return Err("unknown session");
        };
        if !rect.fits_in((browser_size.w, browser_size.h)) {
            return Err("rect outside browser window");
        }
        let adoption = self
            .adoptions
            .iter_mut()
            .find(|adoption| adoption.browser_session == session)
            .expect("adoption checked above");
        adoption.rect = rect;
        self.needs_redraw = true;
        marker!(
            "ADOPTION MOVED session={session} rect={},{},{},{}",
            rect.x,
            rect.y,
            rect.w,
            rect.h
        );
        Ok(())
    }

    /// Resize an adoption: update the rect and send
    /// `xdg_toplevel.configure` to Chromium with the new content size
    /// (`rect.w` x `rect.h`). A compositor configure, never a CDP call.
    pub fn resize_adoption(
        &mut self,
        session: &str,
        rect: ContentRect,
    ) -> Result<(), AdoptionError> {
        let Some((browser_size, chromium_id)) = self
            .adoption_for_session(session)
            .map(|adoption| (adoption.browser_surface_id, adoption.chromium_surface_id))
            .and_then(|(browser_id, chromium_id)| {
                self.registry
                    .get(browser_id)
                    .map(|record| (record.window.geometry().size, chromium_id))
            })
        else {
            return Err("unknown session");
        };
        if !rect.fits_in((browser_size.w, browser_size.h)) {
            return Err("rect outside browser window");
        }
        let adoption = self
            .adoptions
            .iter_mut()
            .find(|adoption| adoption.browser_session == session)
            .expect("adoption checked above");
        adoption.rect = rect;
        let Some(toplevel) = self
            .registry
            .get(chromium_id)
            .map(|record| record.toplevel.clone())
        else {
            return Err("unknown session");
        };
        toplevel.with_pending_state(|state| {
            state.size = Some(smithay::utils::Size::from((rect.w, rect.h)));
        });
        toplevel.send_configure();
        self.needs_redraw = true;
        marker!(
            "ADOPTION RESIZED session={session} size={}x{} rect={},{},{},{}",
            rect.w,
            rect.h,
            rect.x,
            rect.y,
            rect.w,
            rect.h
        );
        Ok(())
    }

    /// Drop an adoption record. The Chromium window returns to
    /// ordinary compositor placement (cascade) as a standalone window
    /// again — releasing an adoption never destroys a surface.
    pub fn release_adoption(&mut self, session: &str) -> Result<(), AdoptionError> {
        let Some(index) = self
            .adoptions
            .iter()
            .position(|adoption| adoption.browser_session == session)
        else {
            return Err("unknown session");
        };
        let adoption = self.adoptions.remove(index);
        if let Some(record) = self.registry.get(adoption.chromium_surface_id) {
            let window = record.window.clone();
            self.place_window(&window);
        }
        self.needs_redraw = true;
        marker!(
            "ADOPTION RELEASED session={session} chromium={}",
            adoption.chromium_surface_id
        );
        Ok(())
    }

    /// Drop adoptions whose Browser or Chromium surface is gone
    /// (`gone` is the surface ids the registry reconciliation is
    /// about to drop). A dead Chromium surface notifies the Runtime
    /// over the bridge socket with the `chromium_died` event and the
    /// Browser window stays open; a closed Browser window drops its
    /// adoption and asks the adopted Chromium to close with it — a
    /// close is not a release (release is the only path that leaves
    /// the Chromium as a standalone window), so no orphan window
    /// lingers if the Runtime is slow to kill the process.
    pub fn reconcile_adoptions(&mut self, gone: &[u64]) {
        if self.adoptions.is_empty() || gone.is_empty() {
            return;
        }
        let mut dropped: Vec<String> = Vec::new();
        for adoption in self.adoptions.clone() {
            if gone.contains(&adoption.chromium_surface_id) {
                marker!(
                    "ADOPTION CHROMIUM DIED session={} chromium={} pid={}",
                    adoption.browser_session,
                    adoption.chromium_surface_id,
                    adoption.chromium_pid
                );
                let event = serde_json::json!({
                    "v": 1,
                    "event": "chromium_died",
                    "session": adoption.browser_session,
                });
                self.pending_bridge_events.push(event.to_string());
                dropped.push(adoption.browser_session.clone());
            } else if gone.contains(&adoption.browser_surface_id) {
                marker!(
                    "ADOPTION DROPPED session={} reason=browser-closed",
                    adoption.browser_session
                );
                // A close is not a release: ask the adopted Chromium
                // to close with its Browser (the same request the
                // bridge `close` op makes).
                self.close_surface(adoption.chromium_surface_id);
                dropped.push(adoption.browser_session.clone());
            }
        }
        self.adoptions
            .retain(|adoption| !dropped.contains(&adoption.browser_session));
        if !dropped.is_empty() {
            self.needs_redraw = true;
        }
    }

    /// Focus an adopted Chromium surface through its Browser window:
    /// the `Activated` state stays on the Browser toplevel (the
    /// adopted toplevel is never activated as a standalone window),
    /// while keyboard focus goes to the Chromium surface so keys reach
    /// the page. Called on click-to-focus inside the content rect
    /// (see `input.rs`) and as the guard inside `set_keyboard_focus`.
    pub fn focus_adopted(&mut self, browser: &Window, chromium: &Window) {
        let chromium_id = self
            .registry
            .records()
            .iter()
            .find(|record| record.window == *chromium)
            .map(|record| record.id);
        if chromium_id == self.keyboard_focus_id {
            return;
        }
        let serial = self.next_serial();
        let browser_surface_id = browser
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().id());
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            let is_browser = browser_surface_id
                .as_ref()
                .is_some_and(|id| *id == toplevel.wl_surface().id());
            toplevel.with_pending_state(|state| {
                if is_browser {
                    state.states.set(ToplevelState::Activated);
                } else {
                    state.states.unset(ToplevelState::Activated);
                }
            });
            toplevel.send_configure();
        }
        let focus = chromium.wl_surface().map(|surface| surface.into_owned());
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, focus, serial);
        self.keyboard_focus_id = chromium_id;
        self.needs_redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_fits_inside_window() {
        let rect = ContentRect {
            x: 0,
            y: 152,
            w: 800,
            h: 148,
        };
        assert!(rect.fits_in((800, 300)));
    }

    #[test]
    fn rect_outside_window_is_rejected() {
        // Overflows the right edge.
        let rect = ContentRect {
            x: 100,
            y: 152,
            w: 800,
            h: 148,
        };
        assert!(!rect.fits_in((800, 300)));
        // Overflows the bottom edge.
        let rect = ContentRect {
            x: 0,
            y: 152,
            w: 800,
            h: 149,
        };
        assert!(!rect.fits_in((800, 300)));
        // Negative origin.
        let rect = ContentRect {
            x: -1,
            y: 0,
            w: 100,
            h: 100,
        };
        assert!(!rect.fits_in((800, 300)));
        // Zero area.
        let rect = ContentRect {
            x: 0,
            y: 0,
            w: 0,
            h: 100,
        };
        assert!(!rect.fits_in((800, 300)));
    }

    #[test]
    fn rect_global_offsets_by_window_position() {
        let rect = ContentRect {
            x: 0,
            y: 152,
            w: 800,
            h: 148,
        };
        let global = rect.global(Point::from((48, 48)));
        assert_eq!((global.loc.x, global.loc.y), (48, 200));
        assert_eq!((global.size.w, global.size.h), (800, 148));
    }
}
