// SPDX-License-Identifier: GPL-3.0-or-later
//! The one output (Stage 2: exactly one).
//!
//! The nested compositor's output is the winit window it runs in. The
//! output is advertised to clients as `wl_output` (plus `xdg-output`,
//! via [`OutputManagerState::new_with_xdg_output`]) so clients can learn
//! the size they are rendering into. Mode setting, integer scale, rotation,
//! and multi-output are Stage 3 / follow-up items — Stage 2 ships a
//! single fixed output at scale 1.

use smithay::{
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::DisplayHandle,
    utils::{Logical, Physical, Size},
};

use crate::server::GenesisState;

/// Create the single output and advertise it as a `wl_output` global.
pub fn create_output(dh: &DisplayHandle, size: Size<i32, Logical>) -> Output {
    // Physical size in millimetres, assuming ~96 dpi. Clients use this
    // only for dpi heuristics; the logical size is what matters.
    let physical = PhysicalProperties {
        size: Size::from((size.w * 254 / 960, size.h * 254 / 960)),
        subpixel: Subpixel::Unknown,
        make: "SevynOS".to_string(),
        model: "Genesis nested output".to_string(),
    };
    let output = Output::new("SEVYN-1".to_string(), physical);
    let mode = Mode {
        size: Size::<i32, Physical>::from((size.w, size.h)),
        refresh: 60000,
    };
    output.change_current_state(Some(mode), None, None, None);
    output.create_global::<GenesisState>(dh);
    output
}
