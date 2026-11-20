// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering helpers shared by the nested and standalone backends.
//!
//! Both backends render the same scene — the compositor-owned `Space`
//! plus popups above their parent toplevel — and both must send
//! `wl_surface.frame` callbacks after presenting, or clients stall
//! waiting for the next frame. Only the present step differs (winit
//! window submit vs. DRM page-flip), so that stays in each backend.

use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement,
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            Kind, Wrap,
        },
        ImportAll, ImportDmaWl, ImportMem, ImportMemWl, Renderer,
    },
    desktop::{layer_map_for_output, PopupManager},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Physical, Point, Size},
    wayland::{
        compositor::{with_surface_tree_downward, SurfaceAttributes, TraversalAction},
        presentation::{PresentationFeedbackCachedState, Refresh},
        seat::WaylandFocus,
    },
};

use crate::{cursor::CURSOR_SIZE, server::GenesisState};

// The render-element type for the standalone (DRM) backend: the
// scene's surface elements plus the built-in default cursor, which is
// a memory-backed element — it cannot be a
// `SpaceRenderElements::Surface`, so the DRM path wraps the scene's
// leaf elements in this enum (Smithay's `render_elements!` pattern,
// as in `anvil`). The nested backend does not use this type: the
// parent compositor draws the cursor there.
//
// The variants are the leaf types, not `SpaceRenderElements` itself:
// the space returns `Surface` elements for layer surfaces and
// `Element(Wrap<...>)` elements for windows, and both convert here.
// (`ImportAll` is only implemented for renderers that also implement
// `ImportMemWl` + `ImportDmaWl`, so those bounds are spelled out
// here and in `drm.rs::render_with`.)
smithay::backend::renderer::element::render_elements! {
    pub DrmRenderElements<R> where R: ImportAll + ImportMem + ImportMemWl + ImportDmaWl;
    Surface=WaylandSurfaceRenderElement<R>,
    Wrapped=Wrap<WaylandSurfaceRenderElement<R>>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

/// Send `wl_surface.frame` callbacks for a surface tree (copied from
/// Smithay's `minimal` example: the compositor must send frame callbacks
/// after presenting, or clients stall waiting for the next frame).
pub fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surface, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

/// Send frame callbacks for every live toplevel, at the given time.
pub fn send_frames(state: &GenesisState, time: u32) {
    for surface in state.xdg_shell_state.toplevel_surfaces() {
        send_frames_surface_tree(surface.wl_surface(), time);
    }
}

/// Collect the render elements for the popups of every mapped window
/// and every mapped layer surface, positioned above their parent
/// toplevel / layer. Popups render above their parent; the scene
/// itself holds only toplevels (layers come from the LayerMap).
pub fn popup_elements<R: Renderer + ImportAll>(
    state: &GenesisState,
    renderer: &mut R,
    scale: f64,
) -> Vec<WaylandSurfaceRenderElement<R>>
where
    R::TextureId: Clone + 'static,
{
    let mut elements = Vec::new();
    for window in state.space.elements() {
        let Some(window_location) = state.space.element_location(window) else {
            continue;
        };
        let Some(wl_surface) = window.wl_surface() else {
            continue;
        };
        collect_popups(renderer, &wl_surface, window_location, scale, &mut elements);
    }
    // Popups whose parent is a layer surface (dock menus, panel
    // popups): the layer's arranged geometry is its location.
    {
        let layer_map = layer_map_for_output(&state.output);
        for layer in layer_map.layers() {
            let Some(geometry) = layer_map.layer_geometry(layer) else {
                continue;
            };
            collect_popups(
                renderer,
                layer.wl_surface(),
                geometry.loc,
                scale,
                &mut elements,
            );
        }
    }
    elements
}

/// The popup elements for one parent surface at `location` (global
/// compositor space).
fn collect_popups<R: Renderer + ImportAll>(
    renderer: &mut R,
    parent: &WlSurface,
    location: Point<i32, Logical>,
    scale: f64,
    elements: &mut Vec<WaylandSurfaceRenderElement<R>>,
) where
    R::TextureId: Clone + 'static,
{
    for (popup, popup_location) in PopupManager::popups_for_surface(parent) {
        let location = location + popup_location;
        elements.extend(render_elements_from_surface_tree(
            renderer,
            popup.wl_surface(),
            Point::<i32, Physical>::from((location.x, location.y)),
            scale,
            1.0,
            smithay::backend::renderer::element::Kind::Unspecified,
        ));
    }
}

/// The cursor element for the client-set cursor surface, if any,
/// positioned at the pointer location minus the hotspot. Returns
/// `None` when no client set a cursor surface (named cursors carry no
/// buffer; hidden means hidden). The caller appends it last so it is
/// topmost — which is also what lets the DRM compositor consider it
/// for the hardware cursor plane.
pub fn cursor_element<R: Renderer + ImportAll>(
    state: &GenesisState,
    renderer: &mut R,
    scale: f64,
) -> Option<WaylandSurfaceRenderElement<R>>
where
    R::TextureId: Clone + 'static,
{
    let (surface, hotspot) = state.cursor_surface.clone()?;
    let location = Point::<f64, Logical>::from((
        state.pointer_location.x - hotspot.x as f64,
        state.pointer_location.y - hotspot.y as f64,
    ));
    let mut elements = render_elements_from_surface_tree(
        renderer,
        &surface,
        Point::<i32, Physical>::from((location.x as i32, location.y as i32)),
        scale,
        1.0,
        smithay::backend::renderer::element::Kind::Cursor,
    );
    elements.pop()
}

/// The built-in default cursor element (the 16x16 arrow from
/// `cursor.rs`), positioned at the pointer location with hotspot
/// (0, 0). Returns `None` when a client-set cursor surface exists
/// (that one wins) or when the cursor is hidden — in both cases the
/// caller draws nothing else. Only the standalone backend calls this;
/// the nested backend leaves the cursor to the parent compositor.
/// Created with [`Kind::Cursor`], so the DRM compositor considers it
/// for the hardware cursor plane when 16x16 fits the device's cursor
/// size, and composites it by GL otherwise.
pub fn default_cursor_element<R: Renderer + ImportAll + ImportMem>(
    state: &GenesisState,
    renderer: &mut R,
) -> Option<MemoryRenderBufferRenderElement<R>>
where
    R::TextureId: Clone + Send + 'static,
{
    if state.cursor_surface.is_some() || state.cursor_hidden {
        return None;
    }
    // The location is the pointer location in logical coordinates,
    // passed through unchanged — the same convention as
    // `cursor_element` above; the render path applies the output
    // scale and transform.
    let location =
        Point::<f64, Physical>::from((state.pointer_location.x, state.pointer_location.y));
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        &state.default_cursor,
        None,
        None,
        Some(Size::<i32, Logical>::from((CURSOR_SIZE, CURSOR_SIZE))),
        Kind::Cursor,
    )
    .ok()
}

/// The output's refresh period, for presentation-time feedback.
fn output_refresh(state: &GenesisState) -> Refresh {
    match state.output.current_mode().map(|mode| mode.refresh) {
        Some(refresh_mhz) if refresh_mhz > 0 => Refresh::Fixed(std::time::Duration::from_nanos(
            1_000_000_000_000 / refresh_mhz as u64,
        )),
        _ => Refresh::Unknown,
    }
}

/// The current `CLOCK_MONOTONIC` time. Presentation feedback is
/// reported in the clock domain the `wp_presentation` global was
/// created with (`CLOCK_MONOTONIC`, see `server.rs`), so the
/// timestamp comes from `clock_gettime`, not from `Instant`
/// (which has no epoch in that domain).
fn monotonic_now() -> std::time::Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32)
}

/// Send presentation-time feedback for every surface that asked for
/// it. Call after a frame is actually presented (queued for
/// page-flip / submitted to the parent), never when a frame was
/// skipped as empty — a surface's feedback is "presented" only when
/// its content reached the screen. Covers toplevels and layer
/// surfaces; popups are part of their parent's tree walk.
pub fn send_presentation_feedback(state: &GenesisState) {
    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind as FeedbackKind;

    let time = monotonic_now();
    let refresh = output_refresh(state);
    let seq = state.frames_presented;
    let output = &state.output;

    let mut surfaces: Vec<WlSurface> = state
        .xdg_shell_state
        .toplevel_surfaces()
        .iter()
        .map(|toplevel| toplevel.wl_surface().clone())
        .collect();
    {
        let layer_map = layer_map_for_output(&state.output);
        surfaces.extend(layer_map.layers().map(|layer| layer.wl_surface().clone()));
    }

    for surface in surfaces {
        with_surface_tree_downward(
            &surface,
            (),
            |_, _, &()| TraversalAction::DoChildren(()),
            |_surface, states, &()| {
                let mut guard = states.cached_state.get::<PresentationFeedbackCachedState>();
                let callbacks = std::mem::take(&mut guard.current().callbacks);
                for callback in callbacks {
                    if callback.clk_id() == libc::CLOCK_MONOTONIC as u32 {
                        callback.presented(output, time, refresh, seq, FeedbackKind::empty());
                    } else {
                        callback.discarded();
                    }
                }
            },
            |_, _, &()| true,
        );
    }
}
