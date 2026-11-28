// SPDX-License-Identifier: GPL-3.0-or-later
//! Rounded toplevel windows (layout manifest §3, polish surface (b)).
//!
//! Genesis rounds the corners of normal-state toplevel windows with a
//! 10px radius (all four corners — the confirmed mockup rounds all four;
//! the manifest text says "10 top corners" and the wording gap is noted
//! in the PR body, not silently reinterpreted). Smithay has no built-in
//! rounded corners, so this is a custom texture shader: the stock
//! `texture.frag` plus a signed-distance rounded-rect edge fade, driven
//! by two uniforms (`u_size`, the texture size in buffer pixels, and
//! `u_radius`, the radius in buffer pixels).
//!
//! Why a compositor shader, not client-side alpha: the rounding must
//! apply to EVERY toplevel uniformly, regardless of what the app draws
//! — a client-side rounded root view would need the RN renderer to
//! support borderRadius clipping (unverified) and every app to opt in.
//!
//! The element is GLES-only by construction. The Pixman fallback never
//! sees it (its scene keeps the plain `Wrapped` element and renders
//! square — see `render.rs`): the `render_elements!` enums are generic
//! over the renderer, and a GLES-only variant cannot live in a generic
//! enum (the macro generates a fully generic `RenderElement<R>` impl).
//! So the nested backend and the DRM GLES path use concrete
//! `<=GlesRenderer` enums, and the DRM Pixman path keeps the generic
//! enum. The swap replaces a toplevel's root `Wrapped` element in
//! place, so stacking order is preserved exactly.
//!
//! The root element is built fresh (not extracted): Smithay's `Wrap<C>`
//! has a private field, so the space's element list cannot be
//! unwrapped. `rounded_element_for` rebuilds the root element from the
//! surface with `WaylandSurfaceRenderElement::from_surface`, taking the
//! location/alpha/kind from the space element being replaced — the
//! placement is exactly the space's, and the buffer import is cached,
//! so the second creation per frame is cheap.

use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        element::{
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
            Element, Id, Kind, RenderElement,
        },
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Texture,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::compositor::with_states,
};

use crate::{diag, marker, server::GenesisState};

/// Corner radius in logical pixels (manifest §3: 10).
pub const CORNER_RADIUS_LOGICAL: f32 = 10.0;

/// The rounded-corner fragment shader: the stock Smithay `texture.frag`
/// plus a rounded-rect edge fade. The `//_DEFINES_` marker line MUST
/// stay: Smithay substitutes `#define EXTERNAL` / `NO_ALPHA` /
/// `DEBUG_FLAGS` there when it compiles each program variant, and the
/// `EXTERNAL` branch (DMA-BUF textures) needs `samplerExternalOES`.
/// The fixed vertex shader supplies `v_coords` (texture coords across
/// the drawn src rect), so `v_coords * u_size` is the fragment's
/// position in buffer pixels. The edge fades over one buffer pixel
/// (anti-aliased) instead of a hard discard.
pub const ROUNDED_FRAG: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision mediump float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 u_size;
uniform float u_radius;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 p = v_coords * u_size;
    vec2 q = abs(p - u_size * 0.5) - (u_size * 0.5 - u_radius);
    float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - u_radius;

    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    color.a *= 1.0 - smoothstep(0.0, 1.0, d);

    gl_FragColor = color;
}
"#;

/// Corner radius in buffer pixels for a buffer scale (1.0 at scale 1,
/// 2.0 at scale 2 — the radius is 10 logical pixels either way).
pub fn corner_radius_buffer_px(buffer_scale: f32) -> f32 {
    CORNER_RADIUS_LOGICAL * buffer_scale
}

/// Whether a toplevel in this state gets rounded corners: only the
/// normal state. Maximized windows fill the work area and fullscreen
/// windows fill the output — neither is rounded.
pub fn is_roundable(maximized: bool, fullscreen: bool) -> bool {
    !maximized && !fullscreen
}

/// Compile the rounded-corner program once (lazily, on the first frame
/// — the renderer does not exist at `GenesisState::new`) and store it
/// on the state. On failure log once and remember it: the compositor
/// keeps rendering square and never retries every frame.
pub fn ensure_rounded_program(state: &mut GenesisState, renderer: &mut GlesRenderer) {
    if state.rounded_program.is_some() || state.rounded_program_failed {
        return;
    }
    match renderer.compile_custom_texture_shader(
        ROUNDED_FRAG,
        &[
            UniformName::new("u_size", UniformType::_2f),
            UniformName::new("u_radius", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            state.rounded_program = Some(program);
            marker!("ROUNDED PROGRAM compiled");
        }
        Err(err) => {
            state.rounded_program_failed = true;
            diag!("rounded-corner program failed to compile ({err:?}); windows stay square");
        }
    }
}

/// The toplevel root surfaces that get rounded this frame: id → surface,
/// for every registry record whose pending state is normal (neither
/// maximized nor fullscreen). The pending state is what the compositor
/// itself tracks.
pub fn roundable_windows(state: &GenesisState) -> HashMap<Id, WlSurface> {
    state
        .registry
        .records()
        .iter()
        .filter(|record| {
            record.toplevel.with_pending_state(|pending| {
                is_roundable(
                    pending.states.contains(ToplevelState::Maximized),
                    pending.states.contains(ToplevelState::Fullscreen),
                )
            })
        })
        .map(|record| {
            (
                Id::from_wayland_resource(record.wl_surface()),
                record.wl_surface().clone(),
            )
        })
        .collect()
}

/// Build the rounded element for one toplevel root surface, or `None`
/// when the surface has no buffer yet, is a solid color, or the import
/// fails — the caller then keeps the plain element and retries next
/// frame. `location` / `alpha` / `kind` come from the space element
/// being replaced, so placement is exactly the space's.
pub fn rounded_element_for(
    renderer: &mut GlesRenderer,
    program: &GlesTexProgram,
    surface: &WlSurface,
    location: Point<i32, Physical>,
    alpha: f32,
    kind: Kind,
) -> Option<RoundedCornersElement> {
    let inner = with_states(surface, |states| {
        WaylandSurfaceRenderElement::from_surface(
            renderer,
            surface,
            states,
            location.to_f64(),
            alpha,
            kind,
        )
    });
    match inner {
        Ok(Some(element)) => RoundedCornersElement::from_root_element(element, program.clone()),
        _ => None,
    }
}

/// A toplevel window's root surface, drawn with the rounded-corner
/// shader. GLES-only: the Pixman path never constructs one.
pub struct RoundedCornersElement {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    program: GlesTexProgram,
    /// Texture size in buffer pixels (the `u_size` uniform).
    size: Size<i32, Buffer>,
    /// Corner radius in buffer pixels (the `u_radius` uniform).
    radius: f32,
}

impl RoundedCornersElement {
    /// Wrap a freshly built root element. Returns `None` when the
    /// element has no texture (solid color: nothing to round — the
    /// plain draw is already correct).
    pub fn from_root_element(
        inner: WaylandSurfaceRenderElement<GlesRenderer>,
        program: GlesTexProgram,
    ) -> Option<Self> {
        let WaylandSurfaceTexture::Texture(texture) = inner.texture() else {
            return None;
        };
        let tex_size = texture.size();
        let logical = inner.buffer_size();
        // The buffer scale is the texture size over the logical buffer
        // size (1 at scale 1, 2 at scale 2). Buffer rotation would swap
        // the axes; SevynOS never rotates buffers, and the width ratio
        // is the right scale for the radius either way.
        let scale = tex_size.w as f32 / logical.w.max(1) as f32;
        Some(Self {
            size: tex_size,
            radius: corner_radius_buffer_px(scale),
            program,
            inner,
        })
    }
}

impl Element for RoundedCornersElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // The corners are transparent: claim nothing opaque, or the
        // renderer would skip drawing the wallpaper underneath them.
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedCornersElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        // Guard: the shader's `v_coords` span the drawn src rect, so
        // it only masks correctly for a full-buffer draw. A cropped
        // src (or a solid color) falls back to the plain draw.
        if src != self.inner.src() {
            return self.inner.draw(frame, src, dst, damage, opaque_regions);
        }
        let WaylandSurfaceTexture::Texture(texture) = self.inner.texture() else {
            return self.inner.draw(frame, src, dst, damage, opaque_regions);
        };
        let uniforms = [
            Uniform::new("u_size", [self.size.w as f32, self.size.h as f32]),
            Uniform::new("u_radius", self.radius),
        ];
        frame.render_texture_from_to(
            texture,
            src,
            dst,
            damage,
            opaque_regions,
            self.inner.transform(),
            self.inner.alpha(),
            Some(&self.program),
            &uniforms,
        )
    }

    // Deliberately NOT delegated: a DRM-plane fast path would bypass
    // the shader, so the rounded element always goes through the GPU
    // composition path (the default `None`).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_scales_with_buffer_scale() {
        assert_eq!(corner_radius_buffer_px(1.0), 10.0);
        assert_eq!(corner_radius_buffer_px(2.0), 20.0);
        assert_eq!(corner_radius_buffer_px(0.5), 5.0);
    }

    #[test]
    fn only_normal_state_rounds() {
        assert!(is_roundable(false, false));
        assert!(!is_roundable(true, false));
        assert!(!is_roundable(false, true));
        assert!(!is_roundable(true, true));
    }

    #[test]
    fn shader_declares_its_uniforms_and_defines_marker() {
        // The `//_DEFINES_` marker is how Smithay injects EXTERNAL /
        // NO_ALPHA / DEBUG_FLAGS; deleting it silently breaks DMA-BUF
        // textures. The uniforms must match the `UniformName`s passed
        // to `compile_custom_texture_shader` and the `Uniform`s passed
        // at draw time.
        assert!(ROUNDED_FRAG.contains("//_DEFINES_"));
        assert!(ROUNDED_FRAG.contains("uniform vec2 u_size;"));
        assert!(ROUNDED_FRAG.contains("uniform float u_radius;"));
        assert!(ROUNDED_FRAG.contains("uniform float alpha;"));
        assert!(ROUNDED_FRAG.contains("varying vec2 v_coords;"));
    }
}
