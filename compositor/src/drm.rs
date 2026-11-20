// SPDX-License-Identifier: GPL-3.0-or-later
//! Standalone backend — DRM/KMS + udev + libseat + libinput (Stage 3).
//!
//! Genesis runs without a parent compositor: one DRM/KMS output at its
//! native mode, libinput devices through the libseat session, and
//! hardware-backed rendering through a GBM swapchain. The shape follows
//! Smithay's own reference compositor (`anvil`, `udev.rs`): a calloop
//! event loop with one source per backend (Wayland socket, display fd,
//! bridge socket, udev, libinput, session notifier, DRM notifier,
//! signals), and a [`DrmCompositor`] that owns damage tracking
//! (`backend::renderer::damage`, via its internal `OutputDamageTracker`),
//! vsync (DRM page-flip), direct scanout where the hardware allows, and
//! the hardware cursor plane.
//!
//! ## Scope (locked)
//!
//! * Single output only: the first connected connector of the first
//!   usable DRM device. Further devices are logged and ignored; no
//!   multi-output, no hotplug mode switching.
//! * Mode setting is the connector's native (preferred, else largest)
//!   mode at startup. Integer scale (`--scale`, default 1) and static
//!   rotation (`--rotate`, set at startup) only.
//! * Rendering is the GLES renderer primary
//!   (`backend::renderer::gles` + `wayland::dmabuf` + GBM allocator)
//!   with the Pixman renderer as fallback when GLES init fails or when
//!   forced off (`--no-gles` / `SEVYN_NO_GLES=1`). The fallback is
//!   provable by booting with GLES forced off: the `READY` marker then
//!   reports `renderer=pixman`.
//! * The hardware cursor plane is used when the DRM compositor can
//!   place the cursor element on it; otherwise the cursor element is
//!   composited by the renderer (the GL fallback). The cursor element
//!   comes from the client-set cursor surface (see `server.rs`); when
//!   no client sets one, the built-in 16x16 default arrow (`cursor.rs`)
//!   is drawn instead, so the pointer is never invisible — same
//!   plane-or-GL rule, via `render::DrmRenderElements`.
//! * No Stage 4 work: no foreign-toplevel manager, no adoption IPC
//!   beyond the existing bridge ops.
//!
//! ## Failure modes
//!
//! * No DRM device, no usable connector, or no libseat session: the
//!   process fails cleanly with a named error on stderr and exit 1 —
//!   never a panic, never a hung black screen. (In this sandbox there
//!   is no `/dev/dri` and no seat, so `--standalone` fails here with
//!   `no DRM device found` / `failed to open libseat session`; the
//!   real standalone proof is worker 3's QEMU/KVM run.)
//! * The claimed output's device is removed while running: the
//!   compositor logs `SEVYN DRM OUTPUT LOST`, asks its surfaces to
//!   close, and exits 1. The supervisor / recovery console owns
//!   restart (see COMPOSITOR_PLAN.md, "Failure without fallback").
//! * SIGTERM / SIGINT: clean exit 0 — every surface is asked to close
//!   first, then the DRM device is dropped, which closes the DRM fd
//!   and releases DRM master (see "DRM master release" below).
//! * Idle: after 5 minutes with no input and nothing inhibiting idle,
//!   the display powers off via `set_dpms(false)`; any input or a new
//!   idle-inhibitor powers it back on (see `idle.rs`).
//!
//! ## DRM master release
//!
//! DRM master is held by the open DRM fd (`DrmDeviceFd::new` acquires
//! it) and released when that fd closes. On a clean exit the device is
//! dropped explicitly; on a panic the default `panic = "unwind"`
//! unwinds the stack, running the same drops — and even on `SIGKILL`
//! the kernel closes the process's fds, which releases master. What
//! cannot be guaranteed is console/VT state after a crash mid-modeset:
//! the kernel VT switch hands the console back to the recovery
//! environment, which is the plan's guarantee, not a compositor one.
//! The panic hook (installed once in `main`, see `logging.rs`) logs
//! `SEVYN CRASH ...` to stdout and `/tmp/genesis.log` (plan item 10).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use calloop::{
    generic::Generic,
    signals::{Signal, Signals},
    EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction,
};
use smithay::{
    backend::{
        allocator::{
            dmabuf::Dmabuf,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent, DrmNode,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles::GlesRenderer, pixman::PixmanRenderer, Bind, Color32F, ImportAll, ImportDma,
            ImportDmaWl, ImportMem, ImportMemWl, Renderer, Texture,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{UdevBackend, UdevEvent},
    },
    desktop::space::SpaceRenderElements,
    reexports::{
        drm::control::{
            connector, crtc, property, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags,
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{Client, Display, DisplayHandle},
    },
    utils::{Logical, Physical, Size, Transform},
    wayland::{dmabuf::DmabufFeedbackBuilder, socket::ListeningSocketSource},
};

use crate::{
    diag, input as input_mod, marker,
    render::{
        cursor_element, default_cursor_element, popup_elements, send_frames,
        send_presentation_feedback, DrmRenderElements,
    },
    runtime_bridge::BridgeServer,
    server::{ClientEvent, ClientState, GenesisState},
};

/// The standalone backend's configuration, from the CLI (see `main.rs`).
pub struct StandaloneConfig {
    pub socket_name: String,
    pub scale: i32,
    pub rotate: Transform,
    pub no_gles: bool,
}

/// Parse `--scale N`: an integer scale, 1 or more.
pub fn parse_scale(text: &str) -> Result<i32, String> {
    let scale: i32 = text
        .parse()
        .map_err(|_| format!("bad --scale {text:?}, expected an integer >= 1"))?;
    if scale < 1 {
        return Err(format!("bad --scale {text:?}, expected an integer >= 1"));
    }
    Ok(scale)
}

/// Parse `--rotate DEG`: a static output rotation, 0/90/180/270.
pub fn parse_rotate(text: &str) -> Result<Transform, String> {
    match text {
        "0" => Ok(Transform::Normal),
        "90" => Ok(Transform::_90),
        "180" => Ok(Transform::_180),
        "270" => Ok(Transform::_270),
        _ => Err(format!("bad --rotate {text:?}, expected 0, 90, 180 or 270")),
    }
}

/// The standalone backend's renderer: GLES primary, Pixman fallback.
///
/// The enum exists so the dmabuf handler (on `GenesisState`, see
/// `server.rs`) can import client dmabufs into whichever renderer is
/// active, and so the render path can dispatch on the concrete
/// renderer type. The renderer lives in `GenesisState` (not in the
/// backend struct) for exactly that reason.
pub enum DrmRenderer {
    /// Boxed: the GLES renderer is ~6 KB, the Pixman one ~64 B —
    /// an unboxed variant would make every `DrmRenderer` 6 KB.
    Gles(Box<GlesRenderer>),
    Pixman(PixmanRenderer),
}

impl DrmRenderer {
    /// `gles` or `pixman`, for the `READY` marker.
    pub fn kind(&self) -> &'static str {
        match self {
            DrmRenderer::Gles(_) => "gles",
            DrmRenderer::Pixman(_) => "pixman",
        }
    }

    /// The dmabuf formats this renderer can import, for the dmabuf
    /// feedback and for the DRM compositor's scanout negotiation.
    pub fn dmabuf_formats(&self) -> FormatSet {
        match self {
            DrmRenderer::Gles(renderer) => renderer.dmabuf_formats(),
            DrmRenderer::Pixman(renderer) => renderer.dmabuf_formats(),
        }
    }

    /// Import a client dmabuf into this renderer, for later sampling.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            DrmRenderer::Gles(renderer) => {
                renderer.import_dmabuf(dmabuf, None)?;
                Ok(())
            }
            DrmRenderer::Pixman(renderer) => {
                renderer.import_dmabuf(dmabuf, None)?;
                Ok(())
            }
        }
    }
}

/// The GBM swapchain allocator behind the DRM compositor.
/// (`GbmAllocator<A>` is generic over the fd type `A`, wrapping a
/// `GbmDevice<A>` internally.)
type DrmSwapchain = GbmAllocator<DrmDeviceFd>;
/// The framebuffer exporter for GBM buffers.
type DrmExporter = GbmFramebufferExporter<DrmDeviceFd>;
/// The DRM compositor for the one output: GBM swapchain, GBM
/// framebuffer exporter, no per-frame user data, GBM on a DRM fd.
type GenesisDrmCompositor = DrmCompositor<DrmSwapchain, DrmExporter, (), DrmDeviceFd>;

/// The one output: its DRM device, the surface the compositor presents
/// on, and the GBM/EGL state behind the renderer.
pub struct DrmOutput {
    /// The DRM device (modesetting, page-flip events). Dropping it closes
    /// the DRM fd, which releases DRM master — see the module docs.
    /// (The GBM device holds its own clone of the fd.)
    device: DrmDevice,
    /// The kernel device id, for matching udev remove events.
    dev_id: libc::dev_t,
    /// The connector driving the output (for DPMS).
    connector: connector::Handle,
    /// The DRM compositor: damage tracking, vsync, scanout, cursor plane.
    compositor: GenesisDrmCompositor,
    /// The EGL display the GLES renderer was created from, if any. Never
    /// read after construction: it is kept alive for as long as the
    /// renderer lives (the renderer is owned by `GenesisState`; the drop
    /// order in `StandaloneLoop` drops the state first).
    #[allow(dead_code)]
    egl_display: Option<EGLDisplay>,
    /// Whether DPMS currently has the panel on.
    dpms_on: bool,
}

impl DrmOutput {
    /// Set the DPMS state of this output's connector: `on = true`
    /// powers the panel on, `on = false` powers it off (DPMS Off).
    /// This is the hook the idle work calls: `poll_idle` (see
    /// `idle.rs`) reports a transition once, and the standalone
    /// loop applies it here. Any input wakes the display by the
    /// caller setting DPMS back on.
    pub fn set_dpms(&mut self, on: bool) -> Result<(), String> {
        let wanted = if on { "On" } else { "Off" };
        let properties = self
            .device
            .get_properties(self.connector)
            .map_err(|err| format!("DPMS: cannot read connector properties: {err}"))?;
        let map = properties
            .as_hashmap(&self.device)
            .map_err(|err| format!("DPMS: cannot resolve connector properties: {err}"))?;
        let Some(info) = map.get("DPMS") else {
            return Err("DPMS: connector has no DPMS property".to_string());
        };
        // The DPMS property is an enum (On/Standby/Suspend/Off, values
        // 0/1/2/3 by the KMS ABI); resolve the wanted name from the
        // enum rather than assuming the numeric value.
        let value = match info.value_type() {
            property::ValueType::Enum(values) => {
                let (raw_values, names) = values.values();
                names
                    .iter()
                    .zip(raw_values.iter())
                    .find(|(name, _)| name.name().to_string_lossy() == wanted)
                    .map(|(_, value)| *value)
                    .ok_or_else(|| format!("DPMS: no {wanted} value in DPMS enum"))?
            }
            _ => {
                return Err("DPMS: connector DPMS property is not an enum".to_string());
            }
        };
        self.device
            .set_property(self.connector, info.handle(), value)
            .map_err(|err| format!("DPMS: cannot set DPMS {wanted}: {err}"))?;
        self.dpms_on = on;
        Ok(())
    }

    /// Whether DPMS currently has the panel on.
    #[allow(dead_code)]
    pub fn dpms_on(&self) -> bool {
        self.dpms_on
    }
}

/// The standalone backend's mutable state, held in the loop data.
struct DrmBackend {
    /// The libseat session: opens devices, owns the VT. Never read
    /// after construction — it is kept alive for the loop's lifetime
    /// because dropping the session closes the seat.
    #[allow(dead_code)]
    session: LibSeatSession,
    /// The one output, once claimed. `None` only before claim (which
    /// happens before the loop starts) — the loop never runs without
    /// one, and losing it stops the loop.
    output: Option<DrmOutput>,
    /// True while the session is paused (VT switched away): rendering
    /// is skipped until activation.
    paused: bool,
}

/// The calloop loop data.
///
/// Field order matters for drop: `state` (which owns the GLES renderer)
/// drops before `drm` (which owns the EGL display the renderer was
/// created from).
struct StandaloneLoop {
    state: GenesisState,
    display: Display<GenesisState>,
    bridge: BridgeServer,
    drm: DrmBackend,
    /// Wayland clients, kept alive for the connection's lifetime.
    clients: Vec<Client>,
    /// Set when the claimed output's device was removed: the loop stops
    /// and `run_standalone` reports it as a fatal error.
    output_lost: bool,
    /// Process start, for the startup budget record.
    started_at: Instant,
    /// When the Wayland socket was up (for `socket_up_ms`).
    socket_up_at: Option<Instant>,
}

/// Open a DRM device node through the session and wrap it in a
/// [`DrmDevice`]. `DrmDeviceFd::new` acquires DRM master for the fd.
/// The notifier is the calloop source for page-flip (VBlank) events.
fn open_drm_device(
    session: &mut LibSeatSession,
    path: &Path,
) -> Result<(DrmDevice, DrmDeviceFd, DrmDeviceNotifier), String> {
    let fd = session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)
        .map_err(|err| format!("cannot open DRM device {}: {err:?}", path.display()))?;
    let device_fd = DrmDeviceFd::new(fd.into());
    let (device, notifier) = DrmDevice::new(device_fd.clone(), true)
        .map_err(|err| format!("cannot use DRM device {}: {err}", path.display()))?;
    Ok((device, device_fd, notifier))
}

/// The output pick from `pick_output`: connector, CRTC, native mode,
/// and the panel's physical size in millimetres, if the connector
/// reports one.
type PickedOutput = (connector::Handle, crtc::Handle, DrmMode, Option<(u32, u32)>);
/// A claimed DRM device with its picked output (device id, device,
/// device fd, device notifier, then the `PickedOutput` fields, flat).
type ClaimedDevice = (
    libc::dev_t,
    DrmDevice,
    DrmDeviceFd,
    DrmDeviceNotifier,
    connector::Handle,
    crtc::Handle,
    DrmMode,
    Option<(u32, u32)>,
);

/// Pick the output on a device: the first connected connector with at
/// least one mode, its native (preferred, else largest) mode, and a
/// CRTC for it. Returns the connector, the CRTC, the mode, and the
/// panel's physical size in millimetres, if the connector reports one.
fn pick_output(device: &DrmDevice) -> Result<PickedOutput, String> {
    let resources = device
        .resource_handles()
        .map_err(|err| format!("cannot list DRM resources: {err}"))?;
    for handle in resources.connectors() {
        let info = match device.get_connector(*handle, false) {
            Ok(info) => info,
            Err(err) => {
                diag!("cannot read connector {handle:?}: {err}");
                continue;
            }
        };
        if info.state() != connector::State::Connected {
            continue;
        }
        if info.modes().is_empty() {
            diag!("connector {handle:?} is connected but reports no modes");
            continue;
        }
        // Native mode: the preferred mode, else the largest by area
        // (ties: the highest refresh).
        let mode = *info
            .modes()
            .iter()
            .max_by_key(|mode| {
                let (w, h) = mode.size();
                let preferred = mode.mode_type().contains(ModeTypeFlags::PREFERRED);
                (preferred, w as u32 * h as u32, mode.vrefresh())
            })
            .expect("modes is non-empty");
        // A CRTC: the first encoder's, else the first CRTC on the device.
        let crtc = info
            .encoders()
            .iter()
            .find_map(|encoder| device.get_encoder(*encoder).ok()?.crtc())
            .or_else(|| resources.crtcs().first().copied())
            .ok_or_else(|| "no CRTC available for the connected connector".to_string())?;
        return Ok((*handle, crtc, mode, info.size()));
    }
    Err("no connected connector with a mode on this DRM device".to_string())
}

/// Create the renderer for a GBM device: GLES primary, Pixman fallback.
/// Returns the renderer and the EGL display to keep alive while a GLES
/// renderer lives (`None` for the Pixman fallback).
fn create_renderer(
    gbm: &GbmDevice<DrmDeviceFd>,
    no_gles: bool,
) -> Result<(DrmRenderer, Option<EGLDisplay>), String> {
    if !no_gles {
        match create_gles_renderer(gbm) {
            Ok((renderer, display)) => {
                return Ok((DrmRenderer::Gles(Box::new(renderer)), Some(display)))
            }
            Err(err) => {
                diag!("GLES renderer init failed ({err}); falling back to Pixman");
            }
        }
    } else {
        diag!("GLES disabled (--no-gles / SEVYN_NO_GLES=1); using Pixman");
    }
    let renderer =
        PixmanRenderer::new().map_err(|err| format!("Pixman renderer init failed: {err:?}"))?;
    Ok((DrmRenderer::Pixman(renderer), None))
}

fn create_gles_renderer(
    gbm: &GbmDevice<DrmDeviceFd>,
) -> Result<(GlesRenderer, EGLDisplay), String> {
    let display = unsafe { EGLDisplay::new(gbm.clone()) }
        .map_err(|err| format!("EGL display on GBM failed: {err}"))?;
    let context = EGLContext::new(&display).map_err(|err| format!("EGL context failed: {err}"))?;
    let renderer = unsafe { GlesRenderer::new(context) }
        .map_err(|err| format!("GlesRenderer failed: {err}"))?;
    Ok((renderer, display))
}

/// Render one frame through the DRM compositor, if there is damage.
/// Returns `true` when a frame was queued (page-flip scheduled).
/// The renderer is taken out of the state for the render and put back
/// afterwards: the borrow cannot overlap the state's own borrow.
fn render_drm(data: &mut StandaloneLoop) -> Result<bool, Box<dyn std::error::Error>> {
    let StandaloneLoop { state, drm, .. } = data;
    if drm.paused {
        return Ok(false);
    }
    let Some(output) = drm.output.as_mut() else {
        return Ok(false);
    };
    let Some(renderer) = state.drm_renderer.take() else {
        return Ok(false);
    };
    let presented = match renderer {
        DrmRenderer::Gles(mut renderer) => {
            let result = render_with(state, &mut output.compositor, &mut *renderer);
            state.drm_renderer = Some(DrmRenderer::Gles(renderer));
            result?
        }
        DrmRenderer::Pixman(mut renderer) => {
            let result = render_with(state, &mut output.compositor, &mut renderer);
            state.drm_renderer = Some(DrmRenderer::Pixman(renderer));
            result?
        }
    };
    if presented && state.frames_presented == 0 {
        let first_frame_ms = state.start_time.elapsed().as_millis();
        marker!(
            "FIRST FRAME PRESENTED output={}x{} backend=drm first_frame_ms={}",
            state.output_size.w,
            state.output_size.h,
            first_frame_ms
        );
        // The startup budget is recorded in the log for acceptance:
        // socket up ≤ 2 s, first frame ≤ 3 s from exec.
        diag!(
            "startup: socket up in {} ms, first frame in {} ms (budgets: 2000 ms / 3000 ms)",
            data.socket_up_at
                .map(|at| at.duration_since(data.started_at).as_millis())
                .unwrap_or(0),
            data.started_at.elapsed().as_millis()
        );
    }
    if presented {
        state.frames_presented += 1;
    }
    state.needs_redraw = false;
    Ok(presented)
}

/// Render the scene with one concrete renderer and queue the frame.
/// `DrmCompositor::render_frame` consults its internal damage tracker
/// (`backend::renderer::damage`'s `OutputDamageTracker`): only damaged
/// regions redraw, and an unchanged scene reports an empty frame, in
/// which case nothing is queued. A queued frame is submitted to the
/// CRTC and presented on the next page-flip (vsync); the cursor
/// element, appended last and marked `Kind::Cursor`, is what the
/// compositor considers for the hardware cursor plane — the
/// client-set surface when there is one, the built-in default arrow
/// otherwise (see `render::default_cursor_element`). Elements are
/// wrapped in `DrmRenderElements` because the default arrow is a
/// memory-backed element, not a surface element.
fn render_with<R>(
    state: &mut GenesisState,
    compositor: &mut GenesisDrmCompositor,
    renderer: &mut R,
) -> Result<bool, Box<dyn std::error::Error>>
where
    R: Renderer + Bind<Dmabuf> + ImportAll + ImportMem + ImportMemWl + ImportDmaWl,
    R::TextureId: Clone + Texture + Send + 'static,
    R::Error: 'static,
{
    let scale = state.output_scale as f64;
    // `render_elements_for_output` takes the frame alpha, not the
    // scale: the space elements are scaled by the output itself.
    // Popups and the cursor take the integer scale explicitly.
    // (Layer surfaces are included in the space elements via the
    // output's LayerMap — see `space_render_elements`.)
    // The space returns `Surface` elements for layer surfaces and
    // `Element(Wrap<...>)` elements for windows; both convert into
    // `DrmRenderElements` leaf variants.
    let mut elements: Vec<DrmRenderElements<R>> = state
        .space
        .render_elements_for_output(renderer, &state.output, 1.0)?
        .into_iter()
        .map(|element| match element {
            SpaceRenderElements::Surface(element) => DrmRenderElements::Surface(element),
            SpaceRenderElements::Element(element) => DrmRenderElements::Wrapped(element),
            SpaceRenderElements::_GenericCatcher(_) => unreachable!(),
        })
        .collect();
    elements.extend(
        popup_elements(state, renderer, scale)
            .into_iter()
            .map(DrmRenderElements::Surface),
    );
    if let Some(cursor) = cursor_element(state, renderer, scale) {
        elements.push(DrmRenderElements::Surface(cursor));
    } else if let Some(cursor) = default_cursor_element(state, renderer) {
        elements.push(DrmRenderElements::Memory(cursor));
    }
    let frame = compositor.render_frame(
        renderer,
        &elements,
        Color32F::new(0.05, 0.05, 0.08, 1.0),
        FrameFlags::DEFAULT,
    )?;
    if frame.is_empty {
        return Ok(false);
    }
    compositor.queue_frame(())?;
    let time = state.start_time.elapsed().as_millis() as u32;
    send_frames(state, time);
    // Presentation-time feedback is sent only for a frame that was
    // actually queued: a surface's content is "presented" only when
    // it reaches the screen.
    send_presentation_feedback(state);
    Ok(true)
}

/// Accept one new Wayland client into the display, recording its pid
/// like the nested backend does.
fn accept_client(data: &mut StandaloneLoop, stream: std::os::unix::net::UnixStream) {
    let mut dh = data.display.handle();
    let client_data = Arc::new(ClientState {
        events: data.state.client_events.clone(),
        ..Default::default()
    });
    match dh.insert_client(stream, client_data) {
        Ok(client) => {
            let pid = client
                .get_credentials(&dh)
                .ok()
                .map(|credentials| credentials.pid as u32);
            if let Some(pid) = pid {
                if let Some(client_state) = client.get_data::<ClientState>() {
                    if let Ok(mut slot) = client_state.pid.lock() {
                        *slot = Some(pid);
                    }
                }
            }
            if let Ok(mut events) = data.state.client_events.lock() {
                events.push(ClientEvent::Connected { pid });
            }
            data.clients.push(client);
        }
        Err(err) => diag!("failed to accept a Wayland client: {err}"),
    }
}

/// Run the standalone compositor until shutdown. Returns `Ok(())` on a
/// clean exit (signal); errors are fatal startup/runtime failures and
/// are reported by `main`.
pub fn run_standalone(config: StandaloneConfig) -> Result<(), Box<dyn std::error::Error>> {
    let started_at = Instant::now();
    // The panic hook is installed once in `main` (`logging.rs`); it
    // only logs, and the panic still unwinds, so the drops below still
    // run and the DRM fd still closes (see the module docs on DRM
    // master). No hook is installed here.

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_err(|_| "XDG_RUNTIME_DIR is not set; the Wayland socket has nowhere to live")?;

    // 1. Session first: the seat name drives udev discovery, and the
    // session opens the DRM and input devices.
    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|err| format!("failed to open libseat session (no seat/logind?): {err:?}"))?;
    let seat = session.seat();

    // 2. Udev discovery for this seat.
    let udev_backend = UdevBackend::new(&seat)
        .map_err(|err| format!("failed to start udev discovery for seat {seat}: {err}"))?;
    let devices: Vec<(libc::dev_t, PathBuf)> = udev_backend
        .device_list()
        .map(|(id, path)| (id, path.to_path_buf()))
        .collect();
    if devices.is_empty() {
        return Err(format!(
            "no DRM device found for seat {seat} (no /dev/dri/card* on this seat)"
        )
        .into());
    }

    // 3. First usable device: one that opens and has a connected
    // connector with a mode. Others are logged and skipped.
    let mut claimed: Option<ClaimedDevice> = None;
    let mut last_error = String::new();
    for (dev_id, path) in &devices {
        let (device, device_fd, notifier) = match open_drm_device(&mut session, path) {
            Ok(triple) => triple,
            Err(err) => {
                diag!("skipping DRM device {}: {err}", path.display());
                last_error = err;
                continue;
            }
        };
        match pick_output(&device) {
            Ok((connector, crtc, mode, physical_mm)) => {
                claimed = Some((
                    *dev_id,
                    device,
                    device_fd,
                    notifier,
                    connector,
                    crtc,
                    mode,
                    physical_mm,
                ));
                break;
            }
            Err(err) => {
                diag!("skipping DRM device {}: {err}", path.display());
                last_error = err;
                continue;
            }
        }
    }
    let Some((dev_id, mut device, device_fd, device_notifier, connector, crtc, mode, physical_mm)) =
        claimed
    else {
        return Err(format!("no usable DRM output on any device: {last_error}").into());
    };
    let (mode_w, mode_h) = mode.size();
    let mode_refresh = mode.vrefresh();
    let mode_size = Size::<i32, Physical>::from((mode_w as i32, mode_h as i32));
    let transformed = config.rotate.transform_size(mode_size);
    let logical_size = Size::<i32, Logical>::from((
        transformed.w / config.scale.max(1),
        transformed.h / config.scale.max(1),
    ));
    let connector_name = device
        .get_connector(connector, false)
        .map(|info| format!("{:?}-{}", info.interface(), info.interface_id()))
        .unwrap_or_else(|_| format!("connector-{connector:?}"));
    diag!(
        "DRM output: {connector_name} native mode {mode_w}x{mode_h}@{mode_refresh} scale={} rotate={:?}",
        config.scale,
        config.rotate
    );

    // 4. The DRM surface for the output.
    let surface = device
        .create_surface(crtc, mode, &[connector])
        .map_err(|err| format!("cannot create DRM surface for {connector_name}: {err}"))?;

    // 5. The Wayland display and the compositor state, with the output
    // at the native mode / scale / rotation.
    let mut display: Display<GenesisState> = Display::new()?;
    let dh: DisplayHandle = display.handle();
    let mut state = GenesisState::new_standalone(
        dh.clone(),
        logical_size,
        mode_size,
        config.scale,
        config.rotate,
        physical_mm,
        (mode_refresh * 1000) as i32,
    );

    // 6. GBM + renderer + DRM compositor.
    let gbm = GbmDevice::new(device_fd.clone())
        .map_err(|err| format!("cannot create GBM device for {connector_name}: {err}"))?;
    let no_gles = config.no_gles
        || std::env::var("SEVYN_NO_GLES")
            .map(|v| v == "1")
            .unwrap_or(false);
    let (renderer, egl_display) = create_renderer(&gbm, no_gles)
        .map_err(|err| format!("no renderer available for {connector_name}: {err}"))?;
    let renderer_kind = renderer.kind();
    let renderer_formats = renderer.dmabuf_formats();
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let import_node = DrmNode::from_file(device_fd.clone()).ok();
    let exporter = GbmFramebufferExporter::new(gbm.clone(), import_node);
    let cursor_size = device.cursor_size();
    let compositor: GenesisDrmCompositor = DrmCompositor::new(
        &state.output,
        surface,
        None,
        allocator,
        exporter,
        [Fourcc::Argb8888, Fourcc::Xrgb8888],
        renderer_formats,
        cursor_size,
        Some(gbm.clone()),
    )
    .map_err(|err| format!("cannot create DRM compositor for {connector_name}: {err:?}"))?;
    state.drm_renderer = Some(renderer);

    // 7. The dmabuf global, so clients can submit zero-copy buffers:
    // feedback built from this device's formats.
    let main_device = device_fd
        .dev_id()
        .map_err(|err| format!("cannot read DRM device id: {err}"))?;
    let feedback = DmabufFeedbackBuilder::new(
        main_device,
        state
            .drm_renderer
            .as_ref()
            .expect("renderer just installed")
            .dmabuf_formats(),
    )
    .build()
    .map_err(|err| format!("cannot build dmabuf feedback: {err}"))?;
    state.dmabuf_global = Some(
        state
            .dmabuf_state
            .create_global_with_default_feedback::<GenesisState>(&dh, &feedback),
    );

    // 8. The bridge socket (same channel as nested mode).
    let bridge = BridgeServer::bind(&runtime_dir)?;

    // 9. The event loop and its sources.
    let mut event_loop: EventLoop<StandaloneLoop> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let stop_signal: LoopSignal = event_loop.get_signal();

    let socket_source = ListeningSocketSource::with_name(&config.socket_name)?;
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();
    loop_handle.insert_source(socket_source, |stream, _, data| {
        accept_client(data, stream);
    })?;

    let display_fd = display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .map_err(|err| format!("cannot duplicate Wayland display poll fd: {err}"))?;
    loop_handle.insert_source(
        Generic::new(display_fd, Interest::READ, CalloopMode::Level),
        |_, _, data: &mut StandaloneLoop| {
            if let Err(err) = data.display.dispatch_clients(&mut data.state) {
                diag!("dispatch_clients failed: {err}");
            }
            if let Err(err) = data.display.flush_clients() {
                diag!("flush_clients failed: {err}");
            }
            Ok(PostAction::Continue)
        },
    )?;

    let bridge_fd = bridge.poll_fd()?;
    loop_handle.insert_source(
        Generic::new(bridge_fd, Interest::READ, CalloopMode::Level),
        |_, _, data: &mut StandaloneLoop| {
            data.bridge.poll(&mut data.state);
            Ok(PostAction::Continue)
        },
    )?;

    // Libinput: pointer/keyboard/touch from the kernel devices, opened
    // through the libseat session.
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat)
        .map_err(|err| format!("libinput cannot take seat {seat}: {err:?}"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context);
    loop_handle.insert_source(libinput_backend, |event, _, data: &mut StandaloneLoop| {
        input_mod::dispatch_libinput_event(&mut data.state, event);
    })?;

    // Udev: further devices are logged and ignored (single output);
    // losing the claimed device stops the loop (see module docs).
    let stop_on_output_lost = stop_signal.clone();
    loop_handle.insert_source(
        udev_backend,
        move |event, _, data: &mut StandaloneLoop| match event {
            UdevEvent::Added { device_id, path } => {
                diag!(
                    "ignoring additional DRM device {} (id {device_id}; single output)",
                    path.display()
                );
            }
            UdevEvent::Changed { device_id } => {
                diag!("DRM device changed: id {device_id}");
            }
            UdevEvent::Removed { device_id } => {
                let lost = data
                    .drm
                    .output
                    .as_ref()
                    .map(|output| output.dev_id == device_id)
                    .unwrap_or(false);
                if lost {
                    marker!("DRM OUTPUT LOST device={device_id}");
                    data.output_lost = true;
                    stop_on_output_lost.stop();
                } else {
                    diag!("unclaimed DRM device removed: id {device_id}");
                }
            }
        },
    )?;

    // Session: VT switch away pauses the device; switch back resumes.
    loop_handle.insert_source(
        session_notifier,
        |event, _, data: &mut StandaloneLoop| match event {
            SessionEvent::PauseSession => {
                if let Some(output) = data.drm.output.as_mut() {
                    output.device.pause();
                }
                data.drm.paused = true;
                diag!("session paused (VT switch away); rendering suspended");
            }
            SessionEvent::ActivateSession => {
                if let Some(output) = data.drm.output.as_mut() {
                    if let Err(err) = output.device.activate(false) {
                        diag!("failed to re-activate DRM device: {err}");
                    }
                }
                data.drm.paused = false;
                data.state.needs_redraw = true;
                diag!("session activated; rendering resumed");
            }
        },
    )?;

    // DRM page-flip completion: acknowledge the presented frame so the
    // compositor can submit the next one. The run callback below
    // renders if anything needs a redraw.
    loop_handle.insert_source(
        device_notifier,
        |event, _, data: &mut StandaloneLoop| match event {
            DrmEvent::VBlank(_crtc) => {
                if let Some(output) = data.drm.output.as_mut() {
                    if let Err(err) = output.compositor.frame_submitted() {
                        diag!("frame_submitted failed: {err:?}");
                    }
                }
            }
            DrmEvent::Error(err) => diag!("DRM device error: {err}"),
        },
    )?;

    // Signals: SIGTERM / SIGINT stop the loop for a clean shutdown.
    let stop_on_signal = stop_signal.clone();
    loop_handle.insert_source(
        Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?,
        move |signal, _, _data: &mut StandaloneLoop| {
            diag!("caught signal {signal:?}; shutting down");
            stop_on_signal.stop();
        },
    )?;

    // Children (the Shell, apps) connect through this socket.
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    let mut loop_data = StandaloneLoop {
        state,
        display,
        bridge,
        drm: DrmBackend {
            session,
            output: Some(DrmOutput {
                device,
                dev_id,
                connector,
                compositor,
                egl_display,
                dpms_on: true,
            }),
            paused: false,
        },
        clients: Vec::new(),
        output_lost: false,
        started_at,
        socket_up_at: None,
    };

    // The READY marker: the socket is up and the output is claimed.
    let socket_up_ms = started_at.elapsed().as_millis();
    loop_data.socket_up_at = Some(Instant::now());
    marker!(
        "READY socket={socket_name} bridge={} output={}x{}@{} backend=drm scale={} renderer={renderer_kind} socket_up_ms={socket_up_ms}",
        loop_data.bridge.path.display(),
        logical_size.w,
        logical_size.h,
        mode_refresh,
        config.scale
    );

    // The first frame goes out immediately (the empty desktop), so the
    // startup budget is measured against a real present, not the first
    // client commit.
    if let Err(err) = render_drm(&mut loop_data) {
        diag!("first frame failed: {err}");
    }

    // TEST-ONLY fault injection (Stage 3 milestone 10: "major crashes
    // produce useful logs"). When the environment variable
    // `SEVYN_TEST_PANIC=1` is set, the compositor deliberately panics
    // here — after READY and after the first frame, so the crash path
    // is exercised on a live DRM session: the panic hook emits the
    // `SEVYN CRASH` marker, the unwind drops the DRM device (releasing
    // DRM master), and the process exits 101. This is test
    // infrastructure, not a product feature: it is off unless the
    // variable is set to exactly `1`, it is never set by init or by
    // the image, and `standalone-check.sh` is its only caller. See
    // `compositor/RECOVERY.md`.
    if std::env::var("SEVYN_TEST_PANIC")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        panic!("SEVYN_TEST_PANIC=1: test-only fault injection after READY");
    }

    // Each dispatch round ends here: drain client events, reconcile the
    // surface registry, pump the idle-notifier's timers, apply any
    // pending DPMS transition, and present if anything changed.
    event_loop.run(None, &mut loop_data, |data| {
        data.state.drain_client_events();
        data.state.reconcile_surfaces();
        data.state.pump_idle_timers();
        // Idle/DPMS (see `idle.rs`): a transition is reported exactly
        // once by `poll_idle`; the hardware call and its errors are
        // this backend's job. A failed DPMS set is logged, never fatal
        // — the display state is then unknown, and the next transition
        // retries it.
        if let Some(on) = data.state.poll_idle() {
            if let Some(output) = data.drm.output.as_mut() {
                match output.set_dpms(on) {
                    Ok(()) => marker!("DPMS {}", if on { "ON" } else { "OFF" }),
                    Err(err) => diag!("DPMS set failed (on={on}): {err}"),
                }
            }
        }
        if data.state.needs_redraw {
            if let Err(err) = render_drm(data) {
                diag!("render failed: {err}");
            }
        }
        // Bridge, every round — not just when its fd is readable. A
        // reply is queued in `BridgeServer::poll` and only flushed on
        // the NEXT poll, so polling only on fd readability leaves a
        // reply stuck in the outbox until some client sends another
        // request: the Shell-side `close` worked (the session stopped)
        // but its {"ok":true} never arrived. The nested backend polls
        // the bridge every round for the same reason (see `backend.rs`).
        data.bridge.poll(&mut data.state);
        // Flush after every dispatch round. The display-fd source only
        // flushes when a client SENDS something; without this, events
        // queued outside a client dispatch — pointer/keyboard events
        // from libinput, presentation feedback from a render, focus
        // changes from reconcile — sit in the socket buffer until some
        // client happens to send a message, and a quiet client never
        // receives them at all. (That is exactly what the first
        // standalone-check run showed: the POINTER BUTTON / KEY markers
        // fired, but neither test client ever logged the events.) The
        // nested backend flushes after every round for the same reason
        // (see `backend.rs`); the standalone backend must do the same.
        // A failed flush is logged, never fatal — a dead client is
        // reaped by the next dispatch.
        if let Err(err) = data.display.flush_clients() {
            diag!("flush_clients failed: {err}");
        }
    })?;

    // Clean shutdown: ask every application to close, flush, drop the
    // bridge socket, then exit. Dropping `loop_data` closes the DRM fd
    // (releasing DRM master) and the libseat session.
    let mut loop_data = loop_data;
    diag!(
        "shutting down with {} Wayland client(s) connected",
        loop_data.clients.len()
    );
    loop_data.state.close_all_surfaces();
    let _ = loop_data.display.dispatch_clients(&mut loop_data.state);
    let _ = loop_data.display.flush_clients();
    let _ = std::fs::remove_file(&loop_data.bridge.path);
    if loop_data.output_lost {
        return Err("DRM output lost (device removed)".into());
    }
    marker!("SHUTDOWN clean");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scale_accepts_positive_integers() {
        assert_eq!(parse_scale("1"), Ok(1));
        assert_eq!(parse_scale("2"), Ok(2));
        assert_eq!(parse_scale("3"), Ok(3));
    }

    #[test]
    fn parse_scale_rejects_non_positive_and_non_numeric() {
        assert!(parse_scale("0").is_err());
        assert!(parse_scale("-1").is_err());
        assert!(parse_scale("1.5").is_err());
        assert!(parse_scale("two").is_err());
        assert!(parse_scale("").is_err());
    }

    #[test]
    fn parse_rotate_accepts_quarter_turns() {
        assert_eq!(parse_rotate("0"), Ok(Transform::Normal));
        assert_eq!(parse_rotate("90"), Ok(Transform::_90));
        assert_eq!(parse_rotate("180"), Ok(Transform::_180));
        assert_eq!(parse_rotate("270"), Ok(Transform::_270));
    }

    #[test]
    fn parse_rotate_rejects_anything_else() {
        assert!(parse_rotate("45").is_err());
        assert!(parse_rotate("360").is_err());
        assert!(parse_rotate("-90").is_err());
        assert!(parse_rotate("ninety").is_err());
    }
}
