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
            utils::CropRenderElement,
            Element, Id, Kind, Wrap,
        },
        ImportAll, ImportDmaWl, ImportMem, ImportMemWl, Renderer,
    },
    desktop::{layer_map_for_output, PopupManager, Window},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
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
    /// A surface element clipped to its adoption rect (Stage 4).
    Cropped=CropRenderElement<WaylandSurfaceRenderElement<R>>,
}

// The render-element type for the nested (winit) backend: the scene's
// surface elements plus the clipped adopted elements. The nested
// backend draws no memory-backed cursor (the parent compositor draws
// it), so there is no `Memory` variant here — the only difference
// from `DrmRenderElements`. (A plain comment, not a doc comment:
// rustdoc does not generate documentation for macro invocations.)
smithay::backend::renderer::element::render_elements! {
    pub NestedRenderElements<R> where R: ImportAll + ImportMem + ImportMemWl + ImportDmaWl;
    Surface=WaylandSurfaceRenderElement<R>,
    Wrapped=Wrap<WaylandSurfaceRenderElement<R>>,
    /// A surface element clipped to its adoption rect (Stage 4).
    Cropped=CropRenderElement<WaylandSurfaceRenderElement<R>>,
}

/// The clipped render elements for every live adoption, one entry per
/// Browser window: the Browser window (for stacking — see
/// [`splice_adopted`]) and the adopted Chromium surface's elements,
/// placed so the Chromium window's GEOMETRY lands at the Browser
/// window's output position plus the rect origin (see
/// [`GenesisState::adopted_buffer_origin`]: the surface tree is
/// positioned by its buffer origin, so the geometry offset is
/// subtracted first) and cropped to the rect (global compositor
/// coordinates, scaled to physical pixels by `scale`).
///
/// An adoption whose Browser window is not mapped, or whose Chromium
/// surface has not committed a buffer yet, contributes an empty
/// element list — it renders nothing this frame, and renders the
/// moment the surface commits.
pub fn adopted_elements<R: Renderer + ImportAll>(
    state: &GenesisState,
    renderer: &mut R,
    scale: f64,
) -> Vec<(
    Window,
    Vec<CropRenderElement<WaylandSurfaceRenderElement<R>>>,
)>
where
    R::TextureId: Clone + 'static,
{
    let mut out = Vec::new();
    for adoption in state.adoptions.clone() {
        let Some(browser) = state
            .registry
            .get(adoption.browser_surface_id)
            .map(|record| record.window.clone())
        else {
            continue;
        };
        let Some(chromium) = state
            .registry
            .get(adoption.chromium_surface_id)
            .map(|record| record.toplevel.wl_surface().clone())
        else {
            continue;
        };
        // The surface tree is positioned by its BUFFER origin; the
        // adoption rect names the GEOMETRY origin — subtract the
        // geometry offset so the window itself (not its shadow
        // margins) lands at the rect origin. See
        // `GenesisState::adopted_buffer_origin`.
        let Some(location) = state.adopted_buffer_origin(&adoption) else {
            out.push((browser, Vec::new()));
            continue;
        };
        let Some(clip) = state.adopted_global_rect(&adoption) else {
            out.push((browser, Vec::new()));
            continue;
        };
        // Logical → physical: the element location and the crop rect
        // are both scaled by the output scale, so they agree in the
        // physical space `CropRenderElement::from_element` expects.
        let physical_location = Point::<i32, Physical>::from((
            (location.x as f64 * scale) as i32,
            (location.y as f64 * scale) as i32,
        ));
        let physical_clip = Rectangle::<i32, Physical>::new(
            Point::from((
                (clip.loc.x as f64 * scale) as i32,
                (clip.loc.y as f64 * scale) as i32,
            )),
            Size::from((
                (clip.size.w as f64 * scale) as i32,
                (clip.size.h as f64 * scale) as i32,
            )),
        );
        let elements = render_elements_from_surface_tree(
            renderer,
            &chromium,
            physical_location,
            scale,
            1.0,
            Kind::Unspecified,
        );
        let cropped: Vec<CropRenderElement<WaylandSurfaceRenderElement<R>>> = elements
            .into_iter()
            .filter_map(|element| {
                CropRenderElement::from_element(element, Scale::from(scale), physical_clip)
            })
            .collect();
        out.push((browser, cropped));
    }
    out
}

/// Splice each adoption's clipped elements into the scene's element
/// list immediately above its Browser window's own elements, so the
/// adopted content renders above the Browser chrome it sits inside
/// and below any window stacked above the Browser window.
///
/// The list `render_elements_for_output` returns is front-to-back
/// (`draw_render_elements` reverses it before drawing, so the first
/// element in the list is drawn last — on top). "Above the Browser
/// window" therefore means *before* the Browser window's first
/// element in the list, after the elements of any window stacked
/// above it. The Browser window's elements are identified by the
/// Wayland-resource ids of its surface tree (a surface element's
/// [`Element::id`] is `Id::from_wayland_resource` of its
/// `wl_surface`). An adoption whose Browser window currently
/// contributes no elements (no buffer committed yet) goes at the
/// front instead — topmost until the Browser window commits and the
/// next render splices it into place.
pub fn splice_adopted<E: Element>(mut elements: Vec<E>, adopted: Vec<(Window, Vec<E>)>) -> Vec<E> {
    for (browser, adopted_elements) in adopted {
        if adopted_elements.is_empty() {
            continue;
        }
        let browser_ids = browser_surface_ids(&browser);
        let insert_at = elements
            .iter()
            .position(|element| browser_ids.contains(element.id()))
            .unwrap_or(0);
        elements.splice(insert_at..insert_at, adopted_elements);
    }
    elements
}

/// The Wayland-resource ids of every surface in a window's tree
/// (toplevel + subsurfaces), for [`splice_adopted`].
fn browser_surface_ids(window: &Window) -> Vec<Id> {
    use smithay::wayland::compositor::get_children;
    let Some(surface) = window.wl_surface().map(|surface| surface.into_owned()) else {
        return Vec::new();
    };
    let mut ids = vec![Id::from_wayland_resource(&surface)];
    let mut stack = vec![surface];
    while let Some(parent) = stack.pop() {
        for child in get_children(&parent) {
            ids.push(Id::from_wayland_resource(&child));
            stack.push(child);
        }
    }
    ids
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

/// Send frame callbacks for every live toplevel and every live layer
/// surface, at the given time. Layer surfaces are clients too: the
/// shell is a layer-shell Background surface, and without a callback
/// after each present it stalls waiting for the next frame — with swap
/// interval 1 the client's second swap blocks forever. Popups are
/// part of their parent's tree walk.
pub fn send_frames(state: &GenesisState, time: u32) {
    for surface in state.xdg_shell_state.toplevel_surfaces() {
        send_frames_surface_tree(surface.wl_surface(), time);
    }
    let layer_map = layer_map_for_output(&state.output);
    for layer in layer_map.layers() {
        send_frames_surface_tree(layer.wl_surface(), time);
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
    // Popups of adopted Chromium windows: the adopted window is not
    // in the Space (see `adoption.rs`), so its popups are collected
    // here at the adopted location instead. The parent location is
    // the BUFFER origin (popup positions are relative to the parent
    // surface), so the same geometry-offset subtraction applies as
    // for the window itself — see `adopted_buffer_origin`.
    for adoption in state.adoptions.clone() {
        let Some(chromium) = state
            .registry
            .get(adoption.chromium_surface_id)
            .and_then(|record| record.window.wl_surface())
        else {
            continue;
        };
        let Some(location) = state.adopted_buffer_origin(&adoption) else {
            continue;
        };
        collect_popups(renderer, &chromium, location, scale, &mut elements);
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
