// SPDX-License-Identifier: GPL-3.0-or-later
//! Input routing — pointer + keyboard, and the Stage 2 focus policy.
//!
//! How input flows in the nested compositor: the parent compositor sends
//! pointer/keyboard events to the winit window; the winit backend turns
//! them into Smithay [`InputEvent`]s; this module hit-tests them against
//! the compositor-owned scene (`Space<Window>`) and routes them to the
//! surface actually under the pointer, or to the keyboard-focused surface.
//!
//! How input flows standalone (Stage 3): libinput delivers pointer /
//! keyboard / touch events from the kernel devices; [`dispatch_libinput_event`]
//! routes them through the *same* hit-test and focus rules. Pointer
//! motion is relative (deltas accumulate, clamped to the output) or
//! absolute (transformed from physical panel pixels through
//! [`physical_to_logical`]); touch follows the pointer rule — down
//! raises and focuses the touched window, motion/up go to the surface
//! under the touch point, and every touch event is framed to the client.
//!
//! Touch was verified against the pointer rule in Stage 3 (not just
//! asserted): `touch_down` calls the same [`hit_test`] as
//! `pointer_button` and raises + click-to-focuses on a hit, `touch_motion`
//! hit-tests the same way `pointer_motion` does, and a touch on empty
//! space delivers to no surface — exactly the pointer's empty-space
//! rule. No fix was needed; the only touch/pointer difference is
//! deliberate (touch motion never moves *pointer* focus).
//!
//! ## Stage 2 focus policy (the default, until the Shell defines its own)
//!
//! * A new toplevel takes keyboard focus and is marked `Activated`
//!   (see `xdg_shell.rs`).
//! * Pointer motion moves *pointer* focus only — the leaf surface under
//!   the pointer (subsurfaces included, via `Window::surface_under`).
//!   Moving the pointer never steals keyboard focus.
//! * Pointer press on a window raises it to the top of the stacking order
//!   and moves *keyboard* focus to it (click-to-focus).
//! * Pointer press on empty space clears pointer focus; keyboard focus is
//!   unchanged.
//! * When the focused toplevel goes away, focus moves to the topmost
//!   remaining window, or to none (see `reconcile_surfaces`).
//!
//! This policy is the compositor's default, not the Shell's policy: the
//! Shell defines window/focus policy for the product (see COMPOSITOR_PLAN.md,
//! "Window / focus policy"), and Genesis is the mechanism. When the Shell
//! arrives, this module is where its policy lands.
//!
//! ## Stage 4 adoption focus policy (see COMPOSITOR_PLAN.md, Stage 4)
//!
//! * Pointer hit-test runs adoption-first: inside a Browser window's
//!   adopted content rect, the pointer hits the adopted Chromium
//!   surface; elsewhere in the window it hits the Browser chrome.
//! * Clicking the content rect focuses the Chromium surface (keys go
//!   to the page) while the `Activated` state stays on the Browser
//!   toplevel; clicking the chrome focuses the Browser app.
//! * The adopted toplevel is never activated except through the
//!   Browser window's focus, and never raised on its own — a press
//!   inside the rect raises the Browser window.
//! * Pointer hover alone never moves keyboard focus (the Stage 2 rule
//!   stands): keys go to the page after a click on the content, not
//!   merely because the pointer is over it. This is the one reading
//!   of the plan's "when the Browser window is focused and the pointer
//!   is over the content rect, keys go to the page" that is consistent
//!   with the hover rule; see `compositor/README.md` for the
//!   ambiguity note.

use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisRelativeDirection, ButtonState, Event, InputEvent,
            KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
            PointerMotionEvent, TouchEvent, TouchSlot,
        },
        libinput::LibinputInputBackend,
        winit::WinitInput,
    },
    desktop::{Window, WindowSurfaceType},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        touch::{DownEvent, MotionEvent as TouchMotion, UpEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Physical, Point, Raw, Serial, Size, Transform},
};

use crate::{diag, marker, server::GenesisState};

/// Map a point from an absolute input device's physical output space
/// (the panel's native pixels, untransformed) into logical compositor
/// space: apply the output transform, then divide by the integer scale.
///
/// The transform direction is the inverse of the render transform
/// (the renderer maps logical → physical with `transform.invert()`),
/// so physical → logical applies `transform` itself. Pure function,
/// unit-tested below.
pub(crate) fn physical_to_logical(
    point: Point<f64, Physical>,
    physical_size: Size<i32, Physical>,
    transform: Transform,
    scale: i32,
) -> Point<f64, Logical> {
    let transformed = transform.transform_point_in(point, &physical_size.to_f64());
    Point::from((transformed.x / scale as f64, transformed.y / scale as f64))
}

/// Clamp a logical pointer location inside the output.
fn clamp_to_output(state: &GenesisState, location: Point<f64, Logical>) -> Point<f64, Logical> {
    Point::from((
        location.x.clamp(0.0, state.output_size.w as f64),
        location.y.clamp(0.0, state.output_size.h as f64),
    ))
}

/// Hit-test the pointer location against the scene. Returns the window,
/// the leaf surface under the pointer, and the leaf surface's origin in
/// **global compositor space** — or `None` when the pointer is over
/// empty space.
///
/// Stage 4 (adoption) runs first: a position inside a Browser window's
/// adopted content rect hits the adopted Chromium surface; anywhere
/// else in the window it hits the Browser chrome surfaces (the normal
/// `Space` hit-test below — the adopted window is not in the `Space`,
/// so the space test can only ever return the Browser window there).
/// The adopted window's location is its Browser window's output
/// position plus the rect origin, computed from the live scene.
///
/// The global origin is what Smithay's input handles expect:
/// `PointerHandle::motion` and `TouchHandle::down` both document the
/// focus point as "the coordinates of [the surface's] origin in the
/// global compositor space". (`Window::surface_under` returns the
/// origin in window-local space, so the window's own location is added
/// here; a window-local origin would silently misroute surface-local
/// coordinates for any window not at (0, 0).)
fn hit_test(
    state: &GenesisState,
    location: Point<f64, Logical>,
) -> Option<(Window, WlSurface, Point<i32, Logical>)> {
    for adoption in state.adoptions.clone() {
        let Some(rect) = state.adopted_global_rect(&adoption) else {
            continue;
        };
        let inside = location.x >= rect.loc.x as f64
            && location.x < (rect.loc.x + rect.size.w) as f64
            && location.y >= rect.loc.y as f64
            && location.y < (rect.loc.y + rect.size.h) as f64;
        if !inside {
            continue;
        }
        let Some(chromium) = state
            .registry
            .get(adoption.chromium_surface_id)
            .map(|record| record.window.clone())
        else {
            continue;
        };
        let Some(adopted_location) = state.adopted_location(&adoption) else {
            continue;
        };
        let window_point = location - adopted_location.to_f64();
        let (surface, surface_location) =
            chromium.surface_under(window_point, WindowSurfaceType::ALL)?;
        return Some((chromium, surface, adopted_location + surface_location));
    }
    let (window, window_location) = state.space.element_under(location)?;
    let window_point = location - window_location.to_f64();
    let (surface, surface_location) = window.surface_under(window_point, WindowSurfaceType::ALL)?;
    Some((window.clone(), surface, window_location + surface_location))
}

/// The click-to-focus target for a pressed window: an adopted Chromium
/// window focuses through its Browser window (the `Activated` state
/// stays on the Browser toplevel, keys go to the page); any other
/// window focuses directly. Returns the window to raise: the Browser
/// window for an adopted hit (the whole window, content included,
/// comes forward), the pressed window otherwise.
fn focus_target(state: &GenesisState, window: &Window) -> (Window, Option<Window>) {
    if let Some(browser) = state
        .adoption_for_chromium_window(window)
        .and_then(|adoption| {
            state
                .registry
                .get(adoption.browser_surface_id)
                .map(|record| record.window.clone())
        })
    {
        (browser, Some(window.clone()))
    } else {
        (window.clone(), None)
    }
}

/// Route a pointer motion event: update pointer focus to the leaf surface
/// under the pointer (or clear it over empty space).
pub fn pointer_motion(
    state: &mut GenesisState,
    location: Point<f64, Logical>,
    serial: Serial,
    time: u32,
) {
    // Any input is activity: it resets the DPMS idle clock, tells the
    // idle-notifier, and wakes the display if it was powered off for
    // idle (see `idle.rs`). Every routing function below does the same;
    // these are the choke points for both backends.
    state.note_input_activity();
    state.pointer_location = location;
    let hit = hit_test(state, location);
    let focus = hit
        .as_ref()
        .map(|(_window, surface, surface_location)| (surface.clone(), surface_location.to_f64()));
    // Pointer focus is reported against the toplevel record that owns
    // the leaf surface under the pointer (subsurfaces are not registry
    // entries; the window is).
    let focus_id = hit.as_ref().and_then(|(window, _, _)| {
        state
            .registry
            .records()
            .iter()
            .find(|record| record.window == *window)
            .map(|record| record.id)
    });
    if focus_id != state.pointer_focus_id {
        state.pointer_focus_id = focus_id;
        marker!(
            "POINTER FOCUS surface={} x={} y={}",
            focus_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "(none)".to_string()),
            location.x as i32,
            location.y as i32
        );
    }
    let event = MotionEvent {
        location,
        serial,
        time,
    };
    let pointer = state.pointer.clone();
    pointer.motion(state, focus, &event);
}

/// Route a pointer button event. On press, raise the window under the
/// pointer and move keyboard focus to it (click-to-focus).
///
/// Stage 4: a press inside an adopted content rect raises the Browser
/// window (the whole window, content included) and focuses the
/// Chromium surface through it — the `Activated` state stays on the
/// Browser toplevel (see `GenesisState::focus_adopted`). A press on
/// the chrome focuses the Browser app. The adopted toplevel is never
/// raised or activated on its own.
pub fn pointer_button(
    state: &mut GenesisState,
    button: u32,
    button_state: ButtonState,
    serial: Serial,
    time: u32,
) {
    state.note_input_activity();
    if button_state == ButtonState::Pressed {
        if let Some((window, _surface, _location)) = hit_test(state, state.pointer_location) {
            let (raise, adopted) = focus_target(state, &window);
            state.space.raise_element(&raise, true);
            match adopted {
                Some(chromium) => state.focus_adopted(&raise, &chromium),
                None => state.set_keyboard_focus(Some(&raise)),
            }
        }
    }
    let surface_id = state.pointer_focus_id;
    marker!(
        "POINTER BUTTON button={button} state={} surface={}",
        match button_state {
            ButtonState::Pressed => "pressed",
            ButtonState::Released => "released",
        },
        surface_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );
    let event = ButtonEvent {
        button,
        state: button_state,
        serial,
        time,
    };
    let pointer = state.pointer.clone();
    pointer.button(state, &event);
    pointer.frame(state);
}

/// Route a pointer axis (scroll) event to the surface under the pointer.
pub fn pointer_axis(state: &mut GenesisState, horizontal: f64, vertical: f64, time: u32) {
    state.note_input_activity();
    let pointer = state.pointer.clone();
    pointer.axis(
        state,
        AxisFrame {
            source: None,
            relative_direction: (
                AxisRelativeDirection::Identical,
                AxisRelativeDirection::Identical,
            ),
            time,
            axis: (horizontal, vertical),
            v120: None,
            stop: (false, false),
        },
    );
    pointer.frame(state);
}

/// Route a keyboard event to the keyboard-focused surface. Every key is
/// forwarded (the filter returns `Forward`); Stage 2 defines no
/// compositor keybindings — a global-shortcut story is a follow-up.
pub fn keyboard_key(
    state: &mut GenesisState,
    keycode: Keycode,
    key_state: KeyState,
    serial: Serial,
    time: u32,
) {
    state.note_input_activity();
    let focus_id = state.keyboard_focus_id;
    marker!(
        "KEY key={keycode:?} state={} focus={}",
        match key_state {
            KeyState::Pressed => "pressed",
            KeyState::Released => "released",
        },
        focus_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );
    let keyboard = state.keyboard.clone();
    keyboard.input::<(), _>(state, keycode, key_state, serial, time, |_, _, _| {
        FilterResult::Forward
    });
}

/// Dispatch one winit input event into the routing functions above.
pub fn dispatch_input_event(state: &mut GenesisState, event: InputEvent<WinitInput>) {
    match event {
        InputEvent::Keyboard { event } => {
            let serial = state.next_serial();
            keyboard_key(
                state,
                event.key_code(),
                event.state(),
                serial,
                event.time() as u32,
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let serial = state.next_serial();
            // Absolute positions arrive in the backend's raw coordinate
            // space; at output scale 1 the numbers are the logical ones.
            let position: Point<f64, Raw> = event.position();
            let location = Point::<f64, Logical>::from((position.x, position.y));
            pointer_motion(state, location, serial, event.time() as u32);
        }
        InputEvent::PointerButton { event } => {
            let serial = state.next_serial();
            pointer_button(
                state,
                event.button_code(),
                event.state(),
                serial,
                event.time() as u32,
            );
        }
        InputEvent::PointerAxis { event } => {
            let time = event.time() as u32;
            let horizontal = event.amount(Axis::Horizontal).unwrap_or(0.0);
            let vertical = event.amount(Axis::Vertical).unwrap_or(0.0);
            pointer_axis(state, horizontal, vertical, time);
        }
        _ => {}
    }
}

/// Route a touch-down event: same rule as a pointer press — raise the
/// window under the touch point and move keyboard focus to it
/// (click-to-focus), then deliver the touch to the leaf surface under
/// the point. A touch on empty space delivers to no surface.
pub fn touch_down(
    state: &mut GenesisState,
    location: Point<f64, Logical>,
    slot: TouchSlot,
    serial: Serial,
    time: u32,
) {
    state.note_input_activity();
    let hit = hit_test(state, location);
    if let Some((window, _, _)) = &hit {
        let (raise, adopted) = focus_target(state, window);
        state.space.raise_element(&raise, true);
        match adopted {
            Some(chromium) => state.focus_adopted(&raise, &chromium),
            None => state.set_keyboard_focus(Some(&raise)),
        }
    }
    let focus = hit
        .as_ref()
        .map(|(_window, surface, surface_location)| (surface.clone(), surface_location.to_f64()));
    marker!(
        "TOUCH DOWN slot={:?} x={} y={} surface={}",
        slot,
        location.x as i32,
        location.y as i32,
        hit.as_ref()
            .and_then(|(window, _, _)| state
                .registry
                .records()
                .iter()
                .find(|record| record.window == *window)
                .map(|record| record.id))
            .map(|id| id.to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );
    let event = DownEvent {
        slot,
        location,
        serial,
        time,
    };
    let touch = state.touch.clone();
    touch.down(state, focus, &event);
    touch.frame(state);
}

/// Route a touch-motion event to the surface under the touch point.
/// The touch grab owns the slot once it is down, so motion never
/// re-focuses; the hit-test here only supplies the surface under the
/// point (and its origin) for the grab's bookkeeping / DnD targets,
/// as `TouchHandle::motion` documents. The location is the touch
/// point in logical space.
pub fn touch_motion(
    state: &mut GenesisState,
    location: Point<f64, Logical>,
    slot: TouchSlot,
    time: u32,
) {
    state.note_input_activity();
    let focus = hit_test(state, location)
        .map(|(_window, surface, surface_location)| (surface, surface_location.to_f64()));
    let event = TouchMotion {
        slot,
        location,
        time,
    };
    let touch = state.touch.clone();
    touch.motion(state, focus, &event);
    touch.frame(state);
}

/// Route a touch-up event, then frame the sequence to the client.
pub fn touch_up(state: &mut GenesisState, slot: TouchSlot, serial: Serial, time: u32) {
    state.note_input_activity();
    let event = UpEvent { slot, serial, time };
    let touch = state.touch.clone();
    touch.up(state, &event);
    touch.frame(state);
}

/// Route a touch-cancel event (the touch sequence is void).
pub fn touch_cancel(state: &mut GenesisState) {
    let touch = state.touch.clone();
    touch.cancel(state);
}

/// Dispatch one libinput input event into the routing functions above.
/// This is the standalone backend's input path; it shares the hit-test
/// and focus policy with the nested winit path — only the event source
/// and the coordinate spaces differ:
///
/// * relative pointer motion arrives as deltas, accumulated onto the
///   pointer location and clamped to the output;
/// * absolute pointer motion and touch positions arrive in physical
///   panel pixels and are mapped through [`physical_to_logical`]
///   (output transform, then integer scale).
pub fn dispatch_libinput_event(state: &mut GenesisState, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::Keyboard { event } => {
            let serial = state.next_serial();
            keyboard_key(
                state,
                event.key_code(),
                event.state(),
                serial,
                event.time_msec(),
            );
        }
        InputEvent::PointerMotion { event } => {
            let serial = state.next_serial();
            let delta = event.delta();
            let location = clamp_to_output(
                state,
                state.pointer_location + Point::<f64, Logical>::from((delta.x, delta.y)),
            );
            pointer_motion(state, location, serial, event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let serial = state.next_serial();
            let physical = Point::<f64, Physical>::from((
                event.x_transformed(state.output_physical_size.w),
                event.y_transformed(state.output_physical_size.h),
            ));
            let location = clamp_to_output(
                state,
                physical_to_logical(
                    physical,
                    state.output_physical_size,
                    state.output_transform,
                    state.output_scale,
                ),
            );
            pointer_motion(state, location, serial, event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            let serial = state.next_serial();
            pointer_button(
                state,
                event.button_code(),
                event.state(),
                serial,
                event.time_msec(),
            );
        }
        InputEvent::PointerAxis { event } => {
            let time = event.time_msec();
            let horizontal = event.amount(Axis::Horizontal).unwrap_or(0.0);
            let vertical = event.amount(Axis::Vertical).unwrap_or(0.0);
            pointer_axis(state, horizontal, vertical, time);
        }
        InputEvent::TouchDown { event } => {
            let serial = state.next_serial();
            let physical = Point::<f64, Physical>::from((
                event.x_transformed(state.output_physical_size.w),
                event.y_transformed(state.output_physical_size.h),
            ));
            let location = clamp_to_output(
                state,
                physical_to_logical(
                    physical,
                    state.output_physical_size,
                    state.output_transform,
                    state.output_scale,
                ),
            );
            touch_down(state, location, event.slot(), serial, event.time_msec());
        }
        InputEvent::TouchMotion { event } => {
            let physical = Point::<f64, Physical>::from((
                event.x_transformed(state.output_physical_size.w),
                event.y_transformed(state.output_physical_size.h),
            ));
            let location = clamp_to_output(
                state,
                physical_to_logical(
                    physical,
                    state.output_physical_size,
                    state.output_transform,
                    state.output_scale,
                ),
            );
            touch_motion(state, location, event.slot(), event.time_msec());
        }
        InputEvent::TouchUp { event } => {
            let serial = state.next_serial();
            touch_up(state, event.slot(), serial, event.time_msec());
        }
        InputEvent::TouchCancel { event } => {
            diag!("touch cancelled slot={:?}", event.slot());
            touch_cancel(state);
        }
        InputEvent::TouchFrame { .. } => {
            // Touch events are framed immediately after each down/motion/
            // up above, so a separate frame event has nothing to add.
        }
        InputEvent::DeviceAdded { device } => {
            diag!("input device added: {device:?}");
        }
        InputEvent::DeviceRemoved { device } => {
            diag!("input device removed: {device:?}");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Size, Transform};

    #[test]
    fn physical_to_logical_normal_scale_1() {
        let size = Size::from((1920, 1080));
        let point = physical_to_logical(Point::from((960.0, 540.0)), size, Transform::Normal, 1);
        assert_eq!((point.x as i32, point.y as i32), (960, 540));
    }

    #[test]
    fn physical_to_logical_scale_2() {
        let size = Size::from((1920, 1080));
        let point = physical_to_logical(Point::from((960.0, 540.0)), size, Transform::Normal, 2);
        assert_eq!((point.x as i32, point.y as i32), (480, 270));
    }

    #[test]
    fn physical_to_logical_rotated_90() {
        // 1920x1080 panel rotated 90°: logical space is 1080x1920.
        // The physical top-left corner maps to the logical top-right.
        let size = Size::from((1920, 1080));
        let point = physical_to_logical(Point::from((0.0, 0.0)), size, Transform::_90, 1);
        assert_eq!((point.x as i32, point.y as i32), (1080, 0));
        let point = physical_to_logical(Point::from((1920.0, 1080.0)), size, Transform::_90, 1);
        assert_eq!((point.x as i32, point.y as i32), (0, 1920));
    }

    #[test]
    fn physical_to_logical_rotated_180() {
        let size = Size::from((1920, 1080));
        let point = physical_to_logical(Point::from((0.0, 0.0)), size, Transform::_180, 1);
        assert_eq!((point.x as i32, point.y as i32), (1920, 1080));
    }
}
