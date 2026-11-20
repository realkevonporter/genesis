# Genesis compositor (Rust, Smithay)

The SevynOS compositor. See `../COMPOSITOR_PLAN.md` for the full plan.

## Language (locked)

Rust end to end, on Smithay 0.7.0 (pinned) — "Rust all the way" (Kevon
2026-11-19). No C core, no wlroots, no meson. The only C in the repo is
the test client, which is a client, not the compositor. See the plan's
"Language: Rust" section for the Smithay-vs-wlroots evaluation and the
live-source coverage check (foreign-toplevel management is the one gap,
with a workaround: Genesis implements the manager global from the
maintained `wayland-protocols-wlr` generated types).

## Stage 2 — minimal nested compositor (shipped)

Stage 2 runs nested inside a parent Wayland compositor (Weston during
development) via Smithay's **winit backend** — the same route Smithay's
own reference compositor (`anvil`) and its `minimal` example use: Genesis
is a Wayland client of the parent, opens one window, and renders into it
with the GLES renderer. No DRM/KMS or libinput is involved yet; the
parent owns the hardware. Standalone is Stage 3.

What Stage 2 serves (and nothing else): core Wayland, `wl_shm`,
`wl_output` (+ `xdg-output`), `wl_seat` (pointer + keyboard), and
`xdg-shell` — one output, one seat. No data-device/clipboard, no
dmabuf, no XWayland, no fractional scaling: those arrive with their
stages (see the plan's per-stage map). Stage 3 adds the protocols
listed under "Stage 3 — protocols" below.

### Layout

```text
compositor/
├── Cargo.toml      # genesis-compositor, binary sevyn-compositor
├── Cargo.lock      # committed; builds are --locked
├── src/
│   ├── main.rs             # CLI: --socket, --size, --standalone, --scale, --rotate, --no-gles
│   ├── server.rs           # GenesisState + protocol handlers
│   ├── backend.rs          # winit backend, main loop, bridge socket
│   ├── drm.rs              # standalone backend: DRM/KMS + libseat + libinput (Stage 3)
│   ├── render.rs           # shared scene rendering (space + popups + cursor)
│   ├── cursor.rs           # the built-in default cursor arrow (Stage 3)
│   ├── idle.rs             # idle policy + DPMS (Stage 3)
│   ├── output.rs           # the one output
│   ├── input.rs            # pointer/keyboard/touch routing + focus policy
│   ├── xdg_shell.rs        # toplevel lifecycle + popups
│   ├── surface_registry.rs # toplevel records (surface -> session)
│   ├── runtime_bridge.rs   # versioned surface <-> session association
│   └── logging.rs          # SEVYN milestone markers (stdout) + /tmp/genesis.log
├── tests/
│   ├── test-client.c       # the M1 test client (one xdg_toplevel, two subsurfaces)
│   └── nested-check.sh     # the Stage 2 nested check (CI + local)
├── RECOVERY.md     # the recovery contract (what the compositor guarantees on failure)
├── ACCEPTANCE.md   # the performance-budget record (memory + startup, measured)
└── protocols/      # SevynOS-owned protocol XML (Stage 4 adoption)
```

Stage 2 enables only the Smithay features the nested backend needs
(`backend_winit`, `wayland_frontend`, `desktop`, `renderer_gl`).
Stage 3 adds `backend_drm`, `backend_gbm`, `backend_libinput`,
`backend_udev`, `backend_session`, `backend_session_libseat`, and
`renderer_pixman` (see `Cargo.toml` for why each exists — notably
`backend_session` alone is only the session _trait_; the libseat
implementation is the separate `backend_session_libseat` feature, and
`backend::drm::compositor` is gated on `backend_gbm`). The standalone
backend also depends on `calloop` directly with its `signals` feature
(Smithay re-exports calloop 0.14 but does not enable `signals`); it is
the same locked 0.14.5 crate Smithay uses.

### Run it

```sh
# Inside a Wayland session (e.g. Weston):
sevyn-compositor [--socket <name>] [--size <WIDTHxHEIGHT>]
```

Defaults: socket `sevyn-compositor` in `$XDG_RUNTIME_DIR`, output
`1280x800`. Clients connect with `WAYLAND_DISPLAY=<name>`. Milestone
markers go to stdout (`SEVYN ...` lines, flushed immediately);
diagnostics go to stderr. SIGTERM / SIGINT shuts down cleanly (exit 0):
every surface is asked to close first, then the compositor exits.

### Focus policy (Stage 2 default)

Until the Shell defines its own window/focus policy, the compositor
uses this default — documented here, not invented silently:

- A new toplevel takes keyboard focus and is marked `Activated`.
- Pointer motion moves pointer focus only, to the leaf surface under
  the pointer (subsurfaces included). It never steals keyboard focus.
- Pointer press on a window raises it and moves keyboard focus to it
  (click-to-focus). Keyboard focus is the **toplevel**, not the leaf
  subsurface — this differs from Weston's leaf focus, and is the
  Smithay/anvil convention: a subsurface is part of its toplevel's
  tree, not independently focusable. Keys still reach the right
  application either way.
- A new toplevel gets an `Activated` configure with **no size
  suggestion** (size `0x0`): the client chooses its size, the
  compositor places it (cascade, down-right from (0, 0), clamped to
  the output). Clients never position themselves — that is the property
  Stage 4 adoption relies on.

### Runtime bridge (v1)

The surface ↔ Runtime-session association, and the Shell-side close
channel, live on a Unix socket at
`$XDG_RUNTIME_DIR/sevyn-compositor.sock` (mode 0600, same-uid peers
only via `SO_PEERCRED`). Messages are newline-delimited JSON, protocol
version 1. Stage 2 ops: `ping`, and `close` (the Shell asking the
compositor to close an application):

```json
{ "v": 1, "op": "close", "session": "runtime:<pid>" }
```

Until the Runtime exists and passes real session ids, a session is
identified by its client pid (`runtime:<pid>`); that derivation is
provisional and documented in `src/runtime_bridge.rs`, not a Runtime
API. Stage 4 extends the same socket with `adopt` / `move` / `resize` /
`release`, and adds pid-binding to the known Runtime pid. The
association itself is also written to stdout as `SESSION ASSOCIATED` /
`SESSION STOPPED` markers — the record the (future) Runtime consumes
to record the stopped state (ADR-0006 milestones 6 and 10).

### The nested check

```sh
compositor/tests/nested-check.sh
```

Boots Xvfb → headless Weston (x11 backend) → `sevyn-compositor` →
the test client, then proves: output created, surface accepted +
associated, a frame presented, pointer routed to the surface under the
pointer, click + key reaching the focused surface, bridge close
exiting the client with the session recorded stopped, and clean
shutdown with no orphaned processes. The check proves function, not
fps — the GF65 is the fps proof (see the plan's 60 fps bar). The same
script runs in CI.

### Not yet (named, not claimed)

- **No Shell, no Runtime.** The Shell (RN host) and the Runtime do not
  exist as clients yet; the test client stands in for an application,
  and the bridge markers are the association record the Runtime will
  consume. Process launch by the compositor is a Stage 3/Runtime item.
- **No adoption.** The manager + list protocols are implemented
  (Stage 4, see below), but the adoption itself — placing an adopted
  surface inside a Browser window's content rect — is the adoption
  track's work and is not here yet.
- **No dmabuf proof, no frame-rate claim.** SHM is the buffer path in
  the nested check; the dmabuf global exists in standalone mode and
  imports into the active renderer, but the zero-copy path and 60 fps
  are proven on the GF65 (Stages 4–5).
- **Rendered-frame orientation is not pixel-verified headless.** Frames
  are proven to flow (frame callbacks + client READY); the flip
  (`Transform::Flipped180`, per Smithay's `minimal` example) is a
  visual proof item for the GF65.

## Stage 3 — standalone backend (DRM/KMS)

`--standalone` runs Genesis without a parent compositor: DRM/KMS
output at the panel's native mode, libinput devices through the
libseat session, GLES rendering with a Pixman fallback. The nested
backend is untouched and remains the default.

### Run it

```sh
sevyn-compositor --standalone [--socket <name>] [--scale N] [--rotate 0|90|180|270] [--no-gles]
```

- `--scale N` — integer output scale, default 1. No fractional scaling.
- `--rotate DEG` — static output rotation, 0/90/180/270, default 0.
  Set at startup; there is no runtime rotation change.
- `--no-gles` — skip the GLES renderer and use Pixman. `SEVYN_NO_GLES=1`
  does the same. The fallback must be provable by booting with GLES
  forced off.
- `--size` is ignored in standalone mode (the output's native mode is
  used); a warning is printed.

Requirements: a DRM device (`/dev/dri/card*` on the seat), a libseat
session (a VT session under logind/seatd), and `$XDG_RUNTIME_DIR`
for the Wayland socket. Without a DRM device the compositor fails
cleanly with a named error and exit 1 — never a panic:

```text
sevyn-compositor: fatal: no DRM device found for seat seat0 (no /dev/dri/card* on this seat)
```

Single output only: the first connected connector with a mode, at its
native (preferred, else largest) mode, on the first encoder's CRTC
else the first CRTC. No hotplug mode switching, no multi-output. If
the claimed device is removed, the compositor logs
`SEVYN DRM OUTPUT LOST` and exits 1; the supervisor/recovery owns
restart.

### Rendering

GLES primary (`backend::renderer::gles` + `wayland::dmabuf` + GBM
allocator); Pixman fallback (`backend::renderer::pixman`) when GLES
init fails or is forced off. Damage tracking is `DrmCompositor`'s
internal `OutputDamageTracker` (`backend::renderer::damage`); vsync
is the DRM page-flip — a queued frame is presented on the next flip,
and the `VBlank` event drives `frame_submitted()`. The scene is the
same as nested (space + popups + cursor, appended last so the DRM
compositor can consider it for the hardware cursor plane — the
client-set surface when there is one, the built-in default arrow
otherwise; `src/render.rs` holds the shared scene code, and the
standalone element type is `DrmRenderElements` because the arrow is
a memory-backed element, not a surface element). The renderer lives in
`GenesisState` (not the backend struct) so the dmabuf handler can
import client dmabufs into it; a dmabuf import with no active renderer
fails so the client falls back to SHM.

### Input

libinput pointer/keyboard/touch through the **same** hit-test/focus
rules as Stage 2's `input.rs` (extended for touch; the focus logic is
not forked). Relative pointer motion arrives as deltas, accumulated
onto the pointer location and clamped to the output; absolute pointer
motion and touch positions arrive in physical panel pixels and are
mapped through `physical_to_logical` (output transform, then integer
scale). Keyboard goes through libxkbcommon as in Stage 2. Touch down
raises + click-to-focuses like a pointer press; touch motion is
delivered through the grab (focus is only set on down). Touch was
verified in Stage 3 to route through the same hit-test/focus rules as
pointer (same `hit_test`, same raise + focus on down, same
empty-space rule) — no fix was needed; see `src/input.rs`.

### Protocols (Stage 3)

All via Smithay 0.7.0, verified against the vendored source
(`smithay-0.7.0/src/wayland/...`, `src/desktop/wayland/layer.rs`).
Every global below is advertised in both modes (nested + standalone)
and was proven by the smoke run (a Wayland client binding each one
and exercising the round-trips — see `compositor/ACCEPTANCE.md` for
what was run and saw; the smoke client itself is scratch in `/tmp`,
not shipped):

| Global                                 | Smithay source                                                           | What Genesis does with it                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| -------------------------------------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `zwlr_layer_shell_v1` v4               | `wayland::shell::wlr_layer::WlrLayerShellState` (`WlrLayerShellHandler`) | Layer surfaces are mapped into the output's `LayerMap`; the scene's `render_elements_for_output` already renders mapped layers (background/bottom below windows, top/overlay above). A new layer surface gets an initial configure; commits re-arrange the map. **The Shell migrating its chrome onto layer surfaces is a follow-up** — the Shell works today with chrome drawn inside its own surface. Known limitation: pointer/keyboard input is not yet routed to layer surfaces (`input.rs` hit-tests the `Space` only), so a click on a layer surface falls through to the toplevel beneath it; that routing lands with the Shell migration. |
| `zwlr_foreign_toplevel_manager_v1` v2 | Genesis-implemented (`src/foreign_toplevel.rs`, from `wayland-protocols-wlr` 0.3.12 `foreign_toplevel::v1` server types) | The manager global Smithay 0.7.0 does not ship (see Stage 4 below). On bind, a `toplevel` event per open toplevel (then title / app_id / output_enter / state / done); the same events as toplevels open, close, or change. Requests act on the named toplevel: `activate` (raise + focus), `close` (existing close path), `set_minimized` / `set_maximized` / `set_fullscreen` (+ unsets), `set_rectangle` (minimize hint only). Direct activation of an adopted surface is IGNORED. |
| `ext-foreign-toplevel-list-v1` v1 | `wayland::foreign_toplevel_list::ForeignToplevelListState` (Smithay ships it) | Wired alongside the manager (Stage 4 below): one list handle per toplevel, title / app_id updated as they change, `closed` when the toplevel goes away. List-only — no requests. |
| `wp_presentation` v2                   | `wayland::presentation::PresentationState`                               | Feedback is requested per surface and marked `presented` after a frame is actually queued/submitted, with the `CLOCK_MONOTONIC` timestamp, the output's fixed refresh, and the frame sequence number (`render::send_presentation_feedback`). A skipped (empty-damage) frame sends no feedback — content is "presented" only when it reaches the screen.                                                                                                                                                                                                                                                                                            |
| `wp_viewporter` v1                     | `wayland::viewporter::ViewporterState`                                   | Advertised; no Genesis-side policy (viewporting is applied by the renderer when it imports the surface).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `wp_content_type_manager_v1` v1        | `wayland::content_type::ContentTypeState`                                | Advertised; the content-type hint is recorded by Smithay and available to the renderer. No Genesis-side policy yet.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `wp_single_pixel_buffer_manager_v1` v1 | `wayland::single_pixel_buffer::SinglePixelBufferState`                   | Advertised; 1x1 solid-color buffers work as ordinary surface buffers.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `xdg_activation_v1` v1                 | `wayland::xdg_activation::XdgActivationState` (`XdgActivationHandler`)   | `request_activation` raises the surface's window and gives it keyboard focus, exactly like a pointer press (click-to-focus). Tokens are accepted as-is — there is no Shell focus policy yet to refuse one.                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `zwp_idle_inhibit_manager_v1` v1       | `wayland::idle_inhibit::IdleInhibitManagerState` (`IdleInhibitHandler`)  | Inhibitors hold the display on (see "Idle / DPMS" below); `IDLE INHIBIT` / `IDLE UNINHIBIT` markers carry the held count.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `ext_idle_notifier_v1` v2              | `wayland::idle_notify::IdleNotifierState` (`IdleNotifierHandler`)        | Clients get `idled` / `resumed` on their own timeouts; every input event feeds `notify_activity`. The notifier's timers run on a dedicated calloop loop owned by the state (see `idle.rs` — neither backend's loop can supply a `LoopHandle<'static, GenesisState>`).                                                                                                                                                                                                                                                                                                                                                                              |
| `wp_cursor_shape_manager_v1` v2        | `wayland::cursor_shape::CursorShapeManagerState`                         | A client-requested shape clears the client-set cursor surface and is recorded; the built-in default arrow is drawn for it (see "Cursor" below). The tablet half of the protocol needs `TabletSeatHandler`, which is implemented as an empty no-op — Genesis advertises **no** tablet global and has no tablet hardware story.                                                                                                                                                                                                                                                                                                                      |

**Not added (Stage 4 / follow-ups, per the plan — left out
deliberately, not forgotten):** pointer-constraints +
relative-pointer (Stage 4), data-device clipboard (Stage 4),
screencopy, input-method + text-input, virtual keyboard, tablet,
session lock, fractional scaling, XWayland. See
`COMPOSITOR_PLAN.md`, "Fully equipped".

### Cursor

Two cases, one rule — the cursor element is always the topmost
element, marked `Kind::Cursor`, so the DRM compositor considers it
for the hardware cursor plane when it fits the device's cursor size
and composites it by GL otherwise:

- **Client-set cursor surface** (`wl_pointer.set_cursor`): rendered
  at the pointer location minus the hotspot, as before.
- **No client cursor:** the built-in default arrow is drawn
  (`src/cursor.rs`): a 16x16 bitmap, white fill with a 1 px black
  outline, hotspot (0, 0) — the tip. It is built once at startup
  into a `MemoryRenderBuffer` and re-used every frame. There is **no
  cursor theme system**: every named shape a client requests via
  `cursor-shape` renders as this same arrow (recorded in the state,
  not varied by). A hidden cursor (`CursorImageStatus::Hidden`) draws
  nothing at all, not even the arrow.

In nested mode the parent compositor draws the cursor and Genesis
draws none of this.

### Idle / DPMS

After 5 minutes (`IDLE_DPMS_TIMEOUT` in `src/idle.rs`, the
`swayidle`-style default) with no input and nothing inhibiting idle,
the display powers off; any input wakes it. The two halves:

- **Client notifications** (`ext-idle-notify-v1`) run on the
  client's own timeout — a player asking for a 10 s notification
  does **not** power the panel off at 10 s. DPMS is the compositor's
  own fixed-timeout policy, not the clients'.
- **Idle-inhibit** (`zwp_idle_inhibit_manager_v1`) holds the display
  on. An inhibitor created while the display is off wakes it. The
  inhibitor list is pruned of dead surfaces on every poll, so a
  client that disconnects without destroying its inhibitor cannot
  hold the display on forever (Smithay only calls `uninhibit` on an
  explicit `Destroy`).

Wiring: every input event (pointer motion/button/axis, keys, touch
down/motion/up) calls `GenesisState::note_input_activity` from the
routing functions in `input.rs` — one choke point for both backends.
Each backend iteration calls `pump_idle_timers()` (the notifier's
timers) and `poll_idle()` (at most one transition per call). In
standalone mode the transition is applied through
`DrmOutput::set_dpms(on)` (the enum value is resolved by name from
the connector's DPMS property, not assumed numerically); a failed
DPMS set is logged and retried on the next transition, never fatal.
In nested mode there is no panel, so a transition is a no-op logged
once as a diagnostic. Markers: `SEVYN DPMS ON` / `SEVYN DPMS OFF`
(standalone), `IDLE INHIBIT count=<n>` / `IDLE UNINHIBIT count=<n>`.

A Settings UI for the timeout is a follow-up; the timeout is a
constant until then.

### Markers and shutdown

Standalone emits `SEVYN READY ... backend=drm output=<WxH@refresh>
scale=<n> renderer=<gles|pixman> socket_up_ms=<n>` and
`SEVYN FIRST FRAME PRESENTED ... first_frame_ms=<n>` (budgets: socket
up ≤ 2 s, first frame ≤ 3 s from exec — recorded in the log at
acceptance). All Stage 2 markers keep working in nested mode.

SIGTERM/SIGINT shuts down cleanly (exit 0): every surface is asked to
close first, then the compositor exits; dropping the DRM device closes
its fd, which releases DRM master. A panic unwinds through the same
drops (default `panic="unwind"`), and even SIGKILL releases master via
the kernel closing the fd. What is _not_ guaranteed is console/VT
state after a crash mid-modeset — the kernel VT switch handing the
console back to the recovery environment is the plan's guarantee, not
a compositor one.

### Logging and exit codes

Markers go to stdout (`SEVYN ...`, flushed immediately) and
diagnostics to stderr — and both are also appended to
`/tmp/genesis.log` (see `src/logging.rs`). The file is **truncated on
start** (rotation choice: one run's log; the supervisor owns history
— copy the file off before restarting if it needs a previous run).
If the file cannot be opened, the compositor keeps running on
stdout/stderr only and says so once on stderr. A panic emits
`SEVYN CRASH panic=... at <file:line:col> (unwinding; DRM master
released by fd close on drop)` on stdout and into the log file, plus
the panic message on stderr. See `compositor/RECOVERY.md` for the
full recovery contract.

Exit codes: **0** clean shutdown (signal or window close),
**1** fatal startup/runtime failure (no DRM device, no seat, DRM
output lost, bridge bind failure — always with a named error on
stderr), **2** usage error (bad CLI argument), **101** panic (Rust
default; the `SEVYN CRASH` marker precedes it).

### The standalone check

```sh
# On the KVM box (45.77.162.169), from the repo root:
cargo build --release --locked          # in compositor/
compositor/tests/standalone-check.sh
```

`compositor/tests/standalone-check.sh` is the Stage 3 acceptance
proof. It boots TWO Ubuntu cloud-image guests under QEMU/KVM and
drives each over SSH. Guest 1 has virtio-gpu and proves milestones
1–8 (no parent compositor, one DRM output at its native mode,
keyboard + pointer input injected over the QMP monitor socket —
absolute pointer via the virtio-tablet, keys via the PS/2 keyboard —
Shell + app stand-ins via the test client with distinct app_ids, both
surfaces committed with presentation-time feedback, focus + bridge
close, clean exit 0 with DRM master released, proven by a second
launch reaching READY). Guest 2 has no virtual GPU at all; it loads
the vkms kernel driver instead, and proves the crash test (the
`SEVYN_TEST_PANIC=1` deliberate panic — see below) and ATTEMPTS the
Pixman fallback (`--no-gles`, `renderer=pixman`). The Pixman run is
NOT PROVEN on this box, honestly: the virtio-gpu plane advertises no
explicit modifiers (modifier Invalid only), and Smithay's Pixman
renderer refuses to bind anything but LINEAR while its DrmCompositor
refuses to allocate a modifier the plane does not advertise. vkms
advertises LINEAR for all its formats, but Mesa GBM reports every
buffer it creates on these virtual drivers as modifier Invalid
(proven directly with a gbm probe on the KVM box: v1/v2/plain+LINEAR
all report Invalid), so the fallback cannot present a frame on any DRM
device available here. The check records that as NOT PROVEN — with the
evidence — and exits 1 after proving everything else, rather than
faking a pass. Real-GPU proof is owed (GF65: Intel GBM reports
LINEAR). The check also proves documentation matching the exercised
CLI/markers/exit codes (milestone 9) and records the
performance-budget numbers (steady-state RSS, socket-up, first-frame)
in `compositor/ACCEPTANCE.md`. The guest-side steps live in
`compositor/tests/standalone-guest.sh`.

**CI decision (Stage 3, worker 3): CI is unchanged for standalone.**
GitHub runners have no KVM and no DRM device, so the standalone check
cannot run there; the nested check still proves function in CI and
`cargo build` already compiles the standalone code. Standalone
verification is KVM-box proof — the script fails fast with a clear
message when run somewhere without `/dev/kvm`. The test client change
(presentation-time feedback) is covered by the existing CI job, which
now also generates the presentation-time protocol.

**Crash test (test-only):** `SEVYN_TEST_PANIC=1` makes the compositor
panic just after `READY` + first frame (`src/drm.rs`). It exists only
so the standalone check can prove milestone 10 against a real crash
record (`SEVYN CRASH` marker, `/tmp/genesis.log` entry, exit 101, DRM
master released by the unwind). It is off unless set to exactly `1`,
never set by init or the image, and only `standalone-check.sh` sets
it. See `compositor/RECOVERY.md`.

### Startup and shutdown (what init must supervise)

- **Start:** `sevyn-compositor [--standalone] [--socket <name>]`.
  The `READY` marker means the Wayland socket is up and (standalone)
  the output is claimed; clients may connect from that point. Init
  must set `$XDG_RUNTIME_DIR` (the socket lives there) and, for
  standalone, provide a VT session (logind/seatd) — without one the
  compositor fails cleanly with exit 1, it does not hang.
- **Stop:** send SIGTERM (or SIGINT). The compositor asks every
  surface to close, flushes, emits `SEVYN SHUTDOWN clean`, and exits 0.
  Init must wait for the exit before assuming the DRM fd (and DRM
  master) is released; on timeout it may SIGKILL — the kernel closes
  the fd and releases master anyway.
- **Supervision:** if the compositor exits non-zero or stops
  responding, init restarts it (see `RECOVERY.md` for the
  socket-wait timeout and the recovery console). The compositor
  never restarts itself and never falls back to another compositor.

### Not yet (standalone, named not claimed)

- **Standalone proof exists (QEMU/KVM, 2026-11-20)** — see "The
  standalone check" above: all ten Stage 3 milestones proven on real
  DRM in a QEMU/KVM guest (virtio-gpu, GLES on llvmpipe), with RSS
  125.8 MB, socket-up 102 ms, first-frame 184 ms (see
  `ACCEPTANCE.md`). One item is NOT PROVEN on this box, not failed:
  the Pixman fallback cannot present a frame on any DRM device
  available here (Mesa GBM reports every buffer Invalid) — real-GPU
  proof is owed (GF65). No frame has been presented on GF65 hardware
  yet; that is still owed, not claimed.
- **VT-switch re-activation unproven:** session pause calls
  `device.pause()` and skips rendering; activate calls
  `device.activate(false)` and redraws — but no VT switch has been
  exercised against this code.
- **First-frame damage behavior is trusted to `OutputDamageTracker`:**
  the first frame is rendered explicitly before the loop starts (empty
  desktop), so startup timing is measured against a real present.
- **Layer-surface input routing** is a follow-up with the Shell
  migration (see "Protocols" above); **cursor-shape** renders every
  named shape as the one built-in arrow; **the DPMS timeout** has no
  Settings UI yet (fixed 5 minutes).

## Stage 4 — foreign-toplevel manager + list (manager track)

Stage 4's listing/control side: `ext-foreign-toplevel-list-v1`
(Smithay ships it) wired alongside a Genesis-implemented
`zwlr_foreign_toplevel_manager_v1` global (v2). Smithay 0.7.0 has no
manager module — the gap the plan names honestly — so the global is
implemented in `src/foreign_toplevel.rs` on top of `wayland-server`,
from the maintained generated types in `wayland-protocols-wlr`
0.3.12 (`foreign_toplevel::v1`, server module; added as a direct
dependency in `Cargo.toml`). The adoption itself — placing a surface
inside a Browser window — is the adoption track's work and lives in
its own section, not here.

### What the global offers

- **Listing.** A client that binds the manager gets a `toplevel`
  event for every open toplevel, each followed by `title`, `app_id`,
  `output_enter` (the one output), `state`, and `done` — and the
  same events as toplevels open, close, or change title / app_id /
  state. The list protocol offers the same toplevels list-only (one
  handle per toplevel; title / app_id updates; `closed` on close).
  The sync runs at the end of every `reconcile_surfaces`, so an
  announcement lags a toplevel's creation by at most one main-loop
  iteration.
- **Control.** Handle requests act on the named toplevel:
  `activate` raises the window and gives it keyboard focus;
  `close` forwards to the existing close path (`send_close`);
  `set_maximized` / `unset_maximized` and `set_fullscreen` /
  `unset_fullscreen` set the corresponding xdg-toplevel state and
  send a configure (with the output size while set, and the window
  moved to (0, 0); the previous location is restored when both are
  unset); `set_minimized` / `unset_minimized` unmaps the window
  from the scene while minimized (and re-maps it on unminimize)
  with a configure sent; `set_rectangle` stores a minimize hint.
  `stop` ends the manager's stream (`finished` is sent back).
  Every request is logged with a `MANAGER ...` marker (see below).
  Minimized is contradictory with visible: activating, maximizing,
  or fullscreening a minimized window restores it first, and
  unmaximizing / unfullscreening a minimized window leaves it
  unmapped.
- **Adopted surfaces.** The manager reports every toplevel,
  adopted or not, with its app_id / title / state. Direct
  activation of an adopted surface via the manager is IGNORED
  (`MANAGER ACTIVATE IGNORED ... adopted`) — adopted surfaces are
  activated only through their Browser window's focus.

### What it deliberately does NOT do

- **No adoption flag in the protocol** — there is none. The Shell
  hides adopted surfaces from the window list because the Runtime
  tells it the toplevel is adopted, not because the manager says
  so.
- **No placement.** No manager request can move, resize, reparent,
  or embed a surface — the protocol has no such request, and
  `set_rectangle` is a minimize hint only, never placement.
  Adoption is compositor-internal scene placement, owned by the
  adoption track.
- **No minimized xdg state.** xdg-shell defines no minimized state,
  so minimization is manager-side bookkeeping, reported in the
  manager `state` array; the configure sent on minimize carries no
  state change.
- **No `invalid_rectangle` protocol error.** A `set_rectangle`
  with a negative width or height is ignored and logged
  (`MANAGER SET_RECTANGLE INVALID ...`); the protocol error is not
  posted.
- **Version 2, not 3.** The upstream XML declares version 3, but
  v3 only adds the `parent` event, which Genesis never sends, so
  the global advertises v2 per the plan.

### Adoption seam (for the adoption track)

`GenesisState::is_adopted(surface_id)` is the query the manager
uses for the ignored-activation rule, backed by a set this module
owns and filled by `GenesisState::note_adopted` /
`note_released`. The adoption track calls those when it adopts /
releases, or replaces the backing with its own registry at
integration — the signature is the contract.
`GenesisState::adopted_browser_window` is a STUB returning `None`
until the adoption track provides the mapping; the manager never
calls it. If the adoption track's interface differs at
integration, the coordinator resolves it.

### Markers

`MANAGER BOUND toplevels=N`, `MANAGER TOPLEVEL id=N app_id=..
title=..`, `MANAGER TOPLEVEL UPDATE ...`, `MANAGER TOPLEVEL
CLOSED id=N`, `MANAGER STATE id=N state=..`,
`MANAGER ACTIVATE id=N` / `MANAGER ACTIVATE IGNORED id=N adopted`
/ `MANAGER ACTIVATE UNKNOWN id=N`, `MANAGER CLOSE id=N ok=..`,
`MANAGER MINIMIZE|MAXIMIZE|FULLSCREEN id=N ...=..`,
`MANAGER SET_RECTANGLE id=N x=.. y=.. width=.. height=..`,
`MANAGER STOPPED`, `MANAGER UNBOUND`.

### Exercising it

```sh
# Build the test client with the wlr + ext-list protocol sources (the
# default build does not generate them; without
# -DHAVE_WLR_FOREIGN_TOPLEVEL --manager prints an error and exits 1):
WLR_XML=~/.cargo/registry/src/*/wayland-protocols-wlr-0.3.12/wlr-protocols/unstable/wlr-foreign-toplevel-management-unstable-v1.xml
LIST_XML=~/.cargo/registry/src/*/wayland-protocols-0.32.13/protocols/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml
wayland-scanner client-header "$WLR_XML" wlr-foreign-toplevel-management-unstable-v1-client-protocol.h
wayland-scanner private-code "$WLR_XML" wlr-foreign-toplevel-management-unstable-v1-protocol.c
wayland-scanner client-header "$LIST_XML" ext-foreign-toplevel-list-v1-client-protocol.h
wayland-scanner private-code "$LIST_XML" ext-foreign-toplevel-list-v1-protocol.c
gcc -DHAVE_WLR_FOREIGN_TOPLEVEL -o sevyn-test-client-manager tests/test-client.c \
    xdg-shell-protocol.c presentation-time-protocol.c \
    wlr-foreign-toplevel-management-unstable-v1-protocol.c \
    ext-foreign-toplevel-list-v1-protocol.c \
    -I. $(pkg-config --cflags --libs wayland-client)

# List the open toplevels (manager + list protocols):
WAYLAND_DISPLAY=<socket> ./sevyn-test-client-manager --manager
# Act on one (env: SEVYN_MANAGER_TARGET_APP_ID, SEVYN_MANAGER_ACTION
# = activate|close|minimize|unminimize|maximize|unmaximize|
#   fullscreen|unfullscreen):
SEVYN_MANAGER_TARGET_APP_ID=sevyn.app SEVYN_MANAGER_ACTION=minimize \
    WAYLAND_DISPLAY=<socket> ./sevyn-test-client-manager --manager
# Markers land in /tmp/sevyn-manager-client.log
# (SEVYN_MANAGER_LOG overrides) and on the console.
```

### Not yet (manager track, named not claimed)

- **Integrated adopted-activation proof.** The ignored-activation
  rule is proven at the manager level with a stubbed adoption
  state (unit test `adopted_set_tracks_note_and_release`); the
  end-to-end proof — a real adopted surface's direct activation
  ignored while its Browser window still activates it — waits on
  the adoption track and is proven at integration.
- **The list protocol's proof rides with the manager client.**
  `--manager` binds `ext-foreign-toplevel-list-v1` too and logs
  `LIST TOPLEVEL ...` / `LIST CLOSED ...` for the same toplevels,
  so the list global is proven live by a real client, not only by
  the compositor's sync. There is no standalone list-only client.

## Build

```sh
cargo build --locked
cargo clippy --locked -- -D warnings
cargo fmt --check
```

Needs: Rust stable (pinned by `../rust-toolchain.toml`), gcc,
pkg-config, libwayland-dev, libxkbcommon-dev, wayland-protocols.
Standalone also needs libudev-dev, libinput-dev, libgbm-dev,
libdrm-dev, and libseat-dev (the `backend_session_libseat` feature
links libseat). The build produces `target/debug/sevyn-compositor`
(`target/release/` with `--release`). The test client builds with gcc

- wayland-scanner (see the `compositor` CI job).
