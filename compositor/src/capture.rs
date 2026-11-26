// SPDX-License-Identifier: GPL-3.0-or-later
//! Screen capture — the bridge `capture` op (relay screen observation,
//! R8; see `~/workspace/sevynos-ai-relay/phase2/SCREEN_DESIGN.md`).
//!
//! The producer half of on-demand still frames: the Runtime (later the
//! relay agent) asks over the bridge socket, Genesis renders the scene
//! once into an offscreen target, reads the pixels back, and writes a
//! single PNG. The relay agent reads that file and streams it over the
//! relay channel; no frame ever travels through the daemon.
//!
//! ## Protocol (additive op within bridge protocol v1)
//!
//! Request: `{"v": 1, "op": "capture"}` — optionally with an output
//! path override for tests: `{"v": 1, "op": "capture", "path":
//! "/tmp/.../latest.png"}`. The override must be an absolute path; a
//! relative or empty one is `"malformed request"`.
//!
//! Reply: the reply is *deferred* one loop iteration — the bridge
//! handler only records the request and the backends resolve it after
//! the next render, so the reply arrives on the same connection
//! shortly after the request. Success:
//! `{"v": 1, "ok": true, "op": "capture", "width": 1280, "height": 800,
//! "path": "/data/screen/latest.png"}`. Refusal / failure:
//! `{"v": 1, "ok": false, "op": "capture", "error": "<reason>"}`.
//!
//! ## Readback (Smithay 0.7.0)
//!
//! Smithay 0.7.0 has no screencopy module and no `read_pixels`
//! helper (verified against the vendored source). The readback that
//! does exist is the `Offscreen` + `ExportMem` pair (see
//! `backend::renderer::Offscreen`: "Usually also implement
//! [`ExportMem`] to receive the framebuffers contents"):
//!
//! 1. `Offscreen::create_buffer(Fourcc::Abgr8888, size)` — an
//!    offscreen texture (GLES) or image (Pixman);
//! 2. `Bind::bind` + `Renderer::render` with `Transform::Normal` —
//!    the scene drawn into it, exactly the element list the normal
//!    frame draws;
//! 3. `ExportMem::copy_framebuffer(target, region, Abgr8888)` +
//!    `map_texture` — the pixels back on the CPU.
//!
//! `Abgr8888` is the readback format on purpose: for GLES it maps to
//! `glReadPixels` with format `RGBA` / `UNSIGNED_BYTE`, and for
//! Pixman to `A8B8G8R8` — in both cases the mapped bytes are R, G, B,
//! A per pixel, which is PNG's RGBA order with no swizzle. The
//! mapped rows arrive top-down on both renderers (GLES: the
//! renderer's texture-target projection and `glReadPixels`' bottom-up
//! read cancel; Pixman images are top-down natively), so no row flip
//! is applied — the capture check (C1) asserts known client colours
//! at known positions, which is what pins the orientation. Note
//! `TextureMapping::flipped()` is NOT consulted here: it is a static
//! sampling-convention flag (`GlesMapping::flipped()` is hardcoded
//! true), not a description of the bytes `copy_framebuffer` returns
//! for a `Transform::Normal` render — flipping on it puts the scene
//! upside-down, which is exactly what the first capture-check run
//! showed.
//!
//! One shared implementation serves both backends and both DRM
//! renderers (`capture_offscreen` is generic over the renderer, its
//! offscreen target, and the element type); the backends only collect
//! their own element list and name their target type
//! (`GlesTexture` / Pixman `Image`). The capture pass is a second,
//! independent render — it never touches the presented frame, the
//! DRM damage tracker, or the page-flip queue.
//!
//! ## S3 — sensitive surfaces (auto-pause / blank)
//!
//! There is no lock / password / sensitive concept in the tree, so
//! the rule is built from what the compositor actually knows: every
//! mapped layer surface's namespace and every toplevel's app id +
//! title (see `sensitive_surface_visible`). The sensitive set is an
//! explicit keyword list, matched on *tokens* (identifiers split on
//! non-alphanumerics and camelCase boundaries, lowercased), never on
//! substrings — substring matching would blank ordinary surfaces
//! ("Clock" and "Block" both contain "lock"; "Author" contains
//! "auth"). The set is keyword-based rather than an exact registry of
//! OS identifiers because SevynOS has no lock screen or auth surface
//! yet: there is no app id or namespace in the tree to name exactly.
//! When such surfaces exist, their identifiers should be checked
//! against this list (a `sevyn-lock` layer, a `sevyn-auth` prompt,
//! a title like "Enter password" all match) and the list extended
//! only deliberately, in this module.
//!
//! The rule is fail-closed: any *live* sensitive surface with a
//! committed buffer blanks the capture even if it is occluded or
//! minimised — the compositor does not track occlusion for this
//! check, and a frame that carries a typed password is a failure of
//! this feature, not an edge case. A refused capture writes nothing
//! and leaves the last good frame untouched; the reply is
//! `{"ok": false, "error": "refused: sensitive surface"}`. The
//! normal desktop is never blanked: the shell background / dock
//! namespace `sevyn-shell` contains no sensitive token.
//!
//! ## S4 — one file, no history
//!
//! The default output is `/data/screen/latest.png`, overwritten on
//! every capture. Nothing else is ever written and no prior frame is
//! kept — the single file *is* the retention policy. The
//! output-path override exists only because the CI user cannot write
//! `/data`; the check proves the write / overwrite / blank logic
//! through the override, and the default path is proven on-device
//! (GF65 gate, downstream).

use std::path::{Path, PathBuf};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::RenderElement, utils::draw_render_elements, Color32F, ExportMem, Frame,
            Offscreen, Renderer,
        },
    },
    desktop::layer_map_for_output,
    utils::{Buffer, Physical, Rectangle, Size, Transform},
};

use crate::server::GenesisState;

/// The default capture output (S4: exactly one file, overwritten on
/// every capture). The relay agent reads this path directly.
pub const DEFAULT_CAPTURE_PATH: &str = "/data/screen/latest.png";

/// A pending capture request, recorded by the bridge handler and
/// resolved by the backend after the next render.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// Where the PNG goes (default [`DEFAULT_CAPTURE_PATH`], or the
    /// request's `path` override for tests).
    pub output_path: PathBuf,
}

/// One captured frame: top-down RGBA bytes, `width * height * 4`
/// long, no row padding.
#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Top-down RGBA, row-major.
    pub rgba: Vec<u8>,
}

/// Render the scene once into an offscreen target and read the pixels
/// back. `elements` is the scene's element list, exactly as the
/// normal frame draws it; `size` is the output's physical size.
///
/// Generic over the renderer `R`, its offscreen target `T`
/// (`GlesTexture` for [`GlesRenderer`], Pixman `Image` for
/// [`PixmanRenderer`]), and the element type `E` — one shared
/// implementation for the nested backend and both DRM renderers.
pub fn capture_offscreen<R, T, E>(
    renderer: &mut R,
    elements: &[E],
    size: Size<i32, Physical>,
) -> Result<CapturedFrame, String>
where
    R: Renderer + Offscreen<T> + ExportMem,
    R::Error: std::fmt::Debug,
    R::TextureId: 'static,
    E: RenderElement<R>,
{
    if size.w <= 0 || size.h <= 0 {
        return Err("capture failed: output has no size".to_string());
    }
    let buffer_size = Size::<i32, Buffer>::from((size.w, size.h));
    let mut buffer = renderer
        .create_buffer(Fourcc::Abgr8888, buffer_size)
        .map_err(|err| format!("capture failed: cannot create offscreen buffer: {err:?}"))?;
    let mut target = renderer
        .bind(&mut buffer)
        .map_err(|err| format!("capture failed: cannot bind offscreen buffer: {err:?}"))?;
    {
        let mut frame = renderer
            .render(&mut target, size, Transform::Normal)
            .map_err(|err| format!("capture failed: cannot start offscreen frame: {err:?}"))?;
        let damage = Rectangle::from_size(size);
        // The same clear colour as the presented frame, so a capture
        // of the empty desktop matches what the user sees.
        frame
            .clear(Color32F::new(0.05, 0.05, 0.08, 1.0), &[damage])
            .map_err(|err| format!("capture failed: clear failed: {err:?}"))?;
        draw_render_elements(&mut frame, 1.0, elements, &[damage])
            .map_err(|err| format!("capture failed: draw failed: {err:?}"))?;
        // As in the backends: the fence is not awaited — the
        // readback below is the synchronisation point.
        let _ = frame
            .finish()
            .map_err(|err| format!("capture failed: finish failed: {err:?}"))?;
    }
    // `finish` does not unbind the target, and `copy_framebuffer`
    // re-makes it current, so the readback runs on the offscreen
    // target even after the frame is done.
    let region = Rectangle::<i32, Buffer>::from_size(buffer_size);
    let mapping = renderer
        .copy_framebuffer(&target, region, Fourcc::Abgr8888)
        .map_err(|err| format!("capture failed: readback failed: {err:?}"))?;
    let bytes = renderer
        .map_texture(&mapping)
        .map_err(|err| format!("capture failed: cannot map readback: {err:?}"))?;
    let width = size.w as usize;
    let height = size.h as usize;
    // Rows are `width * 4` bytes; the stride may be wider (Pixman
    // pads rows), so it is derived from the mapping length, not
    // assumed. (GLES returns exactly `width * height * 4`.) The rows
    // arrive top-down (see the module docs) — no flip.
    let stride = bytes.len() / height.max(1);
    if stride < width * 4 || bytes.len() < stride * height {
        return Err("capture failed: short readback".to_string());
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let start = y * stride;
        rgba.extend_from_slice(&bytes[start..start + width * 4]);
    }
    Ok(CapturedFrame {
        width: size.w as u32,
        height: size.h as u32,
        rgba,
    })
}

/// Encode top-down RGBA bytes as a PNG file at `path`, creating the
/// parent directory when needed (so the default `/data/screen/`
/// exists on first capture). The file is truncated and overwritten —
/// S4 keeps exactly one frame.
pub fn write_png_rgba(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("capture failed: cannot write an empty PNG".to_string());
    }
    if rgba.len() != width as usize * height as usize * 4 {
        return Err("capture failed: pixel buffer does not match the PNG size".to_string());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                format!("capture failed: cannot create {}: {err}", parent.display())
            })?;
        }
    }
    let file = std::fs::File::create(path)
        .map_err(|err| format!("capture failed: cannot create {}: {err}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("capture failed: cannot write PNG header: {err}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|err| format!("capture failed: cannot write PNG data: {err}"))?;
    Ok(())
}

/// Resolve a pending capture from its readback outcome: write the
/// PNG on success, set `state.capture_reply` either way (so the next
/// bridge poll delivers it), and emit the marker. Called by the
/// backends after the render pass that serves the request. A failure
/// writes nothing, so the last good frame stays untouched.
pub fn resolve_capture(
    state: &mut GenesisState,
    request: CaptureRequest,
    outcome: Result<CapturedFrame, String>,
) {
    let path = request.output_path;
    let reply = match outcome {
        Ok(frame) => match write_png_rgba(&path, frame.width, frame.height, &frame.rgba) {
            Ok(()) => {
                crate::marker!(
                    "CAPTURE OK width={} height={} path={}",
                    frame.width,
                    frame.height,
                    path.display()
                );
                crate::runtime_bridge::capture_ok_json(
                    frame.width,
                    frame.height,
                    &path.to_string_lossy(),
                )
            }
            Err(err) => {
                crate::marker!("CAPTURE FAILED reason={err}");
                crate::runtime_bridge::capture_err_json(&err)
            }
        },
        Err(err) => {
            crate::marker!("CAPTURE FAILED reason={err}");
            crate::runtime_bridge::capture_err_json(&err)
        }
    };
    state.capture_reply = Some(reply);
}

/// Fail a pending capture without rendering (the backend cannot
/// render: paused, no output, no renderer). The last good frame is
/// untouched. A no-op when no capture is pending.
pub fn fail_pending_capture(state: &mut GenesisState, reason: &str) {
    if state.capture_request.take().is_some() {
        let err = format!("capture failed: {reason}");
        crate::marker!("CAPTURE FAILED reason={err}");
        state.capture_reply = Some(crate::runtime_bridge::capture_err_json(&err));
    }
}

/// The S3 sensitive tokens, matched on whole tokens (see the module
/// docs). "sign" covers "Sign in" (split into `sign` + `in`); the
/// whole-identifier check in [`is_sensitive_text`] covers the
/// hyphenated "log-in" / "sign-in" / "lock-screen" forms.
const SENSITIVE_TOKENS: &[&str] = &[
    "lock",
    "locked",
    "lockscreen",
    "unlock",
    "password",
    "passcode",
    "pin",
    "auth",
    "authenticate",
    "authentication",
    "login",
    "signin",
    "sign",
    "credential",
    "credentials",
    "secret",
];

/// Multi-word sensitive forms, matched against the identifier with
/// all separators stripped ("log-in" → "login", "lock-screen" →
/// "lockscreen").
const SENSITIVE_JOINED: &[&str] = &["login", "signin", "lockscreen"];

/// Split an identifier into lowercase tokens on non-alphanumerics and
/// camelCase boundaries ("sevyn-lock" → ["sevyn", "lock"];
/// "enterPassword" → ["enter", "password"]).
fn sensitive_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // camelCase boundary: a lowercase letter followed by an
            // uppercase one starts a new token.
            if ch.is_uppercase() && prev_lower && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            prev_lower = ch.is_lowercase();
            current.extend(ch.to_lowercase());
        } else {
            prev_lower = false;
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// True when the identifier names a sensitive surface: any token is a
/// sensitive token, or the identifier with separators stripped is a
/// sensitive joined form.
fn is_sensitive_text(text: &str) -> bool {
    if sensitive_tokens(text)
        .iter()
        .any(|token| SENSITIVE_TOKENS.contains(&token.as_str()))
    {
        return true;
    }
    let joined: String = text
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    SENSITIVE_JOINED.contains(&joined.as_str())
}

/// The S3 rule: is a sensitive surface visible right now?
/// Returns a human-readable description of the first one found
/// (`"layer namespace=sevyn-lock"` / `"toplevel app_id=... title=..."`),
/// or `None` when the scene is safe to capture.
///
/// What counts as visible: a mapped layer surface (the LayerMap only
/// holds mapped layers), or a toplevel with a committed buffer
/// (`has_buffer`). Fail-closed on purpose: an occluded or minimised
/// sensitive surface still refuses — the compositor does not track
/// occlusion, and refusing costs a frame while leaking costs a
/// password.
pub fn sensitive_surface_visible(state: &GenesisState) -> Option<String> {
    {
        let layer_map = layer_map_for_output(&state.output);
        for layer in layer_map.layers() {
            let namespace = layer.namespace();
            if is_sensitive_text(namespace) {
                return Some(format!("layer namespace={namespace}"));
            }
        }
    }
    for record in state.registry.records() {
        if !record.has_buffer {
            continue;
        }
        if is_sensitive_text(&record.app_id) || is_sensitive_text(&record.title) {
            return Some(format!(
                "toplevel app_id={} title={}",
                record.app_id, record.title
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_tokens_split_identifiers() {
        assert_eq!(sensitive_tokens("sevyn-lock"), vec!["sevyn", "lock"]);
        assert_eq!(sensitive_tokens("enterPassword"), vec!["enter", "password"]);
        assert_eq!(sensitive_tokens("Lock Screen"), vec!["lock", "screen"]);
    }

    #[test]
    fn sensitive_text_matches_lock_and_password_forms() {
        for text in [
            "sevyn-lock",
            "sevyn-lockscreen",
            "lockScreen",
            "Unlock",
            "Enter password",
            "enterPassword",
            "sevyn-auth",
            "log-in",
            "Sign in",
            "sign-in",
            "PIN",
            "passcode",
        ] {
            assert!(is_sensitive_text(text), "{text} should be sensitive");
        }
    }

    #[test]
    fn sensitive_text_leaves_the_normal_desktop_alone() {
        for text in [
            "sevyn-shell",
            "sevyn-test-layer",
            "sevyn.m1.test",
            "sevyn-m1-test",
            "Clock",
            "Block",
            "Author",
            "Sevyn Browser",
            "chromium",
            "",
        ] {
            assert!(!is_sensitive_text(text), "{text} should not be sensitive");
        }
    }
}
