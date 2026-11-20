// SPDX-License-Identifier: GPL-3.0-or-later
//! The one output (Stage 2: exactly one).
//!
//! The nested compositor's output is the winit window it runs in. The
//! output is advertised to clients as `wl_output` (plus `xdg-output`,
//! via [`OutputManagerState::new_with_xdg_output`]) so clients can learn
//! the size they are rendering into. The standalone backend (Stage 3)
//! uses [`create_output_with`]: the same one output, but at the panel's
//! native mode with an integer scale and a static rotation set at
//! startup. Multi-output stays a follow-up.

use smithay::{
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::DisplayHandle,
    utils::{Logical, Physical, Point, Size, Transform},
};

use crate::server::GenesisState;

/// Create the single output and advertise it as a `wl_output` global.
pub fn create_output(dh: &DisplayHandle, size: Size<i32, Logical>) -> Output {
    create_output_with(
        dh,
        Size::from((size.w, size.h)),
        1,
        Transform::Normal,
        None,
        60000,
        "Genesis nested output",
    )
}

/// Create the single output with an explicit physical mode size, integer
/// scale, and static transform — the standalone backend's output. The
/// logical size clients see is the physical mode size, transformed by
/// `transform`, divided by `scale` (derived by the output itself from
/// the mode, scale, and transform — it is not passed in).
///
/// * `physical_size` — the mode size in physical pixels (the native mode).
/// * `scale` — integer output scale (Stage 3: integer only).
/// * `transform` — static output rotation (Stage 3: set at startup).
/// * `physical_mm` — the panel's physical size in millimetres, when the
///   connector reports one; `None` falls back to the ~96 dpi estimate.
/// * `refresh` — the mode's refresh rate in millihertz.
/// * `model` — the model string advertised to clients.
pub fn create_output_with(
    dh: &DisplayHandle,
    physical_size: Size<i32, Physical>,
    scale: i32,
    transform: Transform,
    physical_mm: Option<(u32, u32)>,
    refresh: i32,
    model: &str,
) -> Output {
    let mm = physical_mm.unwrap_or((
        (physical_size.w * 254 / 960).max(1) as u32,
        (physical_size.h * 254 / 960).max(1) as u32,
    ));
    let physical = PhysicalProperties {
        size: Size::from((mm.0 as i32, mm.1 as i32)),
        subpixel: Subpixel::Unknown,
        make: "SevynOS".to_string(),
        model: model.to_string(),
    };
    let output = Output::new("SEVYN-1".to_string(), physical);
    let mode = Mode {
        size: physical_size,
        refresh,
    };
    output.change_current_state(
        Some(mode),
        Some(transform),
        Some(Scale::Integer(scale)),
        Some(Point::from((0, 0))),
    );
    output.create_global::<GenesisState>(dh);
    output
}
