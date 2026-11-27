// SPDX-License-Identifier: GPL-3.0-or-later
//! Interactive move / resize pointer grabs for xdg-toplevel windows.
//!
//! The host starts an interactive move/resize from a title-bar (44 px)
//! or edge (8 px) pointer press via `xdg_toplevel_move` /
//! `xdg_toplevel_resize` (see the SevynOS host, `wayland.cc`
//! `onPointerButton`). Smithay delivers those requests to
//! `XdgShellHandler::move_request` / `resize_request`; without those
//! handlers the requests are silently dropped (the Smithay 0.7
//! defaults are no-ops) and the window never moves — which is what
//! the SevynOS genesis-check proved on PR #185: drag produced no
//! measurable move, resize width stayed put, and no new
//! `SEVYN SURFACE COMMITTED` size ever arrived.
//!
//! These grabs are the compositor side of that contract. They are
//! started from `xdg_shell.rs` (`move_request` / `resize_request`)
//! with the pointer's current focus kept (`Focus::Keep` — the
//! window stays focused while it is dragged) and end on the left
//! button release. Geometry changes set `needs_redraw`: both
//! backends render only when that flag is set.

use smithay::{
    backend::input::ButtonState,
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::shell::xdg::ToplevelSurface,
};

use crate::{marker, server::GenesisState};

/// Linux input-event code for the left mouse button (`BTN_LEFT`,
/// `0x110`). The grab ends when this button is released.
const BTN_LEFT: u32 = 272;

/// Compositor-side sanity floor for an interactive resize. The host
/// applies its own minimum on configure (`wayland.cc` clamps to
/// `kMinWidth`/`kMinHeight`); this floor only stops a wild drag
/// from producing a degenerate configure.
const MIN_RESIZE_W: i32 = 320;
const MIN_RESIZE_H: i32 = 200;

/// How much of a moved window must stay on the output (pixels).
/// A drag may push a window partly off-screen, but never lose it.
const MIN_VISIBLE_PX: i32 = 100;

/// A grab that is no longer over a live window ends itself instead
/// of moving a dead one.
fn window_alive(data: &GenesisState, window: &Window) -> bool {
    data.space.elements().any(|element| element == window)
}

/// Interactive move: the window follows the pointer from its grab
/// start, clamped so at least `MIN_VISIBLE_PX` stays on the output.
pub struct MoveGrab {
    window: Window,
    start_data: GrabStartData<GenesisState>,
    start_window_location: Point<i32, Logical>,
}

impl MoveGrab {
    pub fn new(
        window: Window,
        start_data: GrabStartData<GenesisState>,
        start_window_location: Point<i32, Logical>,
    ) -> Self {
        Self {
            window,
            start_data,
            start_window_location,
        }
    }
}

impl PointerGrab<GenesisState> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // The client still gets pointer motion (its focus is kept).
        handle.motion(data, focus, event);
        if !window_alive(data, &self.window) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }
        let delta = event.location - self.start_data.location;
        let new_loc: Point<i32, Logical> =
            (self.start_window_location.to_f64() + delta).to_i32_round();
        let output = data.output_size;
        let size = self.window.geometry().size;
        let x = new_loc
            .x
            .clamp(MIN_VISIBLE_PX - size.w, output.w - MIN_VISIBLE_PX);
        let y = new_loc.y.clamp(0, output.h - MIN_VISIBLE_PX);
        data.space.map_element(self.window.clone(), (x, y), false);
        data.needs_redraw = true;
    }

    fn relative_motion(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.button == BTN_LEFT && event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<GenesisState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut GenesisState) {}
}

/// Interactive resize: the window's size follows the pointer from
/// its grab start, per the requested edge(s); left/top edges move
/// the origin with the size. The new size is applied via a
/// configure on every motion (the client acks and commits; the
/// commit is what the check asserts on).
pub struct ResizeGrab {
    window: Window,
    toplevel: ToplevelSurface,
    edges: ResizeEdge,
    start_data: GrabStartData<GenesisState>,
    start_geometry: Rectangle<i32, Logical>,
}

impl ResizeGrab {
    pub fn new(
        window: Window,
        toplevel: ToplevelSurface,
        edges: ResizeEdge,
        start_data: GrabStartData<GenesisState>,
    ) -> Self {
        let start_geometry = window.geometry();
        Self {
            window,
            toplevel,
            edges,
            start_data,
            start_geometry,
        }
    }
}

impl PointerGrab<GenesisState> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, focus, event);
        if !window_alive(data, &self.window) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }
        let dx = (event.location.x - self.start_data.location.x).round() as i32;
        let dy = (event.location.y - self.start_data.location.y).round() as i32;
        let start = self.start_geometry;
        let mut w = start.size.w;
        let mut h = start.size.h;
        match self.edges {
            ResizeEdge::Top => h = start.size.h - dy,
            ResizeEdge::Bottom => h = start.size.h + dy,
            ResizeEdge::Left => w = start.size.w - dx,
            ResizeEdge::Right => w = start.size.w + dx,
            ResizeEdge::TopLeft => {
                w = start.size.w - dx;
                h = start.size.h - dy;
            }
            ResizeEdge::BottomLeft => {
                w = start.size.w - dx;
                h = start.size.h + dy;
            }
            ResizeEdge::TopRight => {
                w = start.size.w + dx;
                h = start.size.h - dy;
            }
            ResizeEdge::BottomRight => {
                w = start.size.w + dx;
                h = start.size.h + dy;
            }
            ResizeEdge::None => {}
            // Non-exhaustive: a future protocol value resizes nothing.
            _ => {}
        }
        let output = data.output_size;
        w = w.clamp(MIN_RESIZE_W, output.w);
        h = h.clamp(MIN_RESIZE_H, output.h);
        // Left/top edges move the origin with the size, so the
        // opposite edge stays put.
        let mut x = start.loc.x;
        let mut y = start.loc.y;
        if matches!(
            self.edges,
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
        ) {
            x = (start.loc.x + (start.size.w - w)).max(0);
        }
        if matches!(
            self.edges,
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
        ) {
            y = (start.loc.y + (start.size.h - h)).max(0);
        }
        let new_size = Size::from((w, h));
        let new_loc = Point::from((x, y));
        if new_size != start.size || new_loc != start.loc {
            self.toplevel.with_pending_state(|state| {
                state.size = Some(new_size);
            });
            self.toplevel.send_configure();
            if new_loc != start.loc {
                data.space.map_element(self.window.clone(), new_loc, false);
            }
            data.needs_redraw = true;
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.button == BTN_LEFT && event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut GenesisState,
        handle: &mut PointerInnerHandle<'_, GenesisState>,
        event: &smithay::input::pointer::GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<GenesisState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut GenesisState) {}
}

/// Build the grab-start data for a move/resize request: the
/// pointer's current focus + location, started by the left button.
pub fn grab_start_data(
    pointer: &smithay::input::pointer::PointerHandle<GenesisState>,
) -> GrabStartData<GenesisState> {
    GrabStartData {
        focus: pointer
            .current_focus()
            .map(|surface| (surface, pointer.current_location())),
        button: BTN_LEFT,
        location: pointer.current_location(),
    }
}

/// Start an interactive move grab on `pointer`, keeping the current
/// focus. Used by `xdg_shell.rs` `move_request`.
pub fn start_move_grab(data: &mut GenesisState, window: Window, serial: smithay::utils::Serial) {
    let Some(start_location) = data.space.element_location(&window) else {
        return;
    };
    let pointer = data.pointer.clone();
    let start_data = grab_start_data(&pointer);
    marker!("MOVE GRAB START window={:?}", window);
    pointer.set_grab(
        data,
        MoveGrab::new(window, start_data, start_location),
        serial,
        Focus::Keep,
    );
}

/// Start an interactive resize grab on `pointer`, keeping the
/// current focus. Used by `xdg_shell.rs` `resize_request`.
pub fn start_resize_grab(
    data: &mut GenesisState,
    window: Window,
    toplevel: ToplevelSurface,
    edges: ResizeEdge,
    serial: smithay::utils::Serial,
) {
    let pointer = data.pointer.clone();
    let start_data = grab_start_data(&pointer);
    marker!("RESIZE GRAB START window={:?} edges={:?}", window, edges);
    pointer.set_grab(
        data,
        ResizeGrab::new(window, toplevel, edges, start_data),
        serial,
        Focus::Keep,
    );
}
