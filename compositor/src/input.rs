// SPDX-License-Identifier: GPL-3.0-or-later
//! Input routing — pointer + keyboard, and the Stage 2 focus policy.
//!
//! How input flows in the nested compositor: the parent compositor sends
//! pointer/keyboard events to the winit window; the winit backend turns
//! them into Smithay [`InputEvent`]s; this module hit-tests them against
//! the compositor-owned scene (`Space<Window>`) and routes them to the
//! surface actually under the pointer, or to the keyboard-focused surface.
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

use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisRelativeDirection, ButtonState, Event, InputEvent,
            KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
        },
        winit::WinitInput,
    },
    desktop::{Window, WindowSurfaceType},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Raw, Serial},
};

use crate::{marker, server::GenesisState};

/// Hit-test the pointer location against the scene. Returns the window,
/// the leaf surface under the pointer, and the surface-local point — or
/// `None` when the pointer is over empty space.
fn hit_test(
    state: &GenesisState,
    location: Point<f64, Logical>,
) -> Option<(Window, WlSurface, Point<i32, Logical>)> {
    let (window, window_location) = state.space.element_under(location)?;
    let window_point = location - window_location.to_f64();
    let (surface, surface_location) = window.surface_under(window_point, WindowSurfaceType::ALL)?;
    Some((window.clone(), surface, surface_location))
}

/// Route a pointer motion event: update pointer focus to the leaf surface
/// under the pointer (or clear it over empty space).
pub fn pointer_motion(
    state: &mut GenesisState,
    location: Point<f64, Logical>,
    serial: Serial,
    time: u32,
) {
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
pub fn pointer_button(
    state: &mut GenesisState,
    button: u32,
    button_state: ButtonState,
    serial: Serial,
    time: u32,
) {
    if button_state == ButtonState::Pressed {
        if let Some((window, _surface, _location)) = hit_test(state, state.pointer_location) {
            state.space.raise_element(&window, true);
            state.set_keyboard_focus(Some(&window));
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
