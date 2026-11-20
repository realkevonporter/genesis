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
dmabuf, no layer-shell, no XWayland, no fractional scaling: those arrive
with their stages (see the plan's per-stage map).

### Layout

```text
compositor/
├── Cargo.toml      # genesis-compositor, binary sevyn-compositor
├── Cargo.lock      # committed; builds are --locked
├── src/
│   ├── main.rs             # CLI: --socket, --size
│   ├── server.rs           # GenesisState + protocol handlers
│   ├── backend.rs          # winit backend, main loop, bridge socket
│   ├── output.rs           # the one output
│   ├── input.rs            # pointer/keyboard routing + focus policy
│   ├── xdg_shell.rs        # toplevel lifecycle + popups
│   ├── surface_registry.rs # toplevel records (surface -> session)
│   ├── runtime_bridge.rs   # versioned surface <-> session association
│   └── logging.rs          # SEVYN milestone markers (stdout)
├── tests/
│   ├── test-client.c       # the M1 test client (one xdg_toplevel, two subsurfaces)
│   └── nested-check.sh     # the Stage 2 nested check (CI + local)
└── protocols/      # SevynOS-owned protocol XML (Stage 4 adoption)
```

Only the Smithay features Stage 2 needs are enabled
(`backend_winit`, `wayland_frontend`, `desktop`, `renderer_gl` — see
`Cargo.toml`): the nested build does not link DRM, libinput, libseat,
or GBM at all.

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
- **No adoption.** `foreign_toplevel_list` is not advertised and no
  manager global is implemented — that is Stage 4 (see the plan's
  honest gap: Smithay 0.7.0 ships the list protocol but not
  `zwlr_foreign_toplevel_manager_v1`).
- **No dmabuf proof, no frame-rate claim.** SHM is the buffer path in
  the nested check; the zero-copy dmabuf path and 60 fps are proven on
  the GF65 (Stages 4–5).
- **No cursor of our own.** In nested mode the parent compositor draws
  the cursor. A compositor-owned cursor (hardware plane, GL fallback,
  cursor-shape protocol) is Stage 3.
- **Rendered-frame orientation is not pixel-verified headless.** Frames
  are proven to flow (frame callbacks + client READY); the flip
  (`Transform::Flipped180`, per Smithay's `minimal` example) is a
  visual proof item for the GF65.

## Build

```sh
cargo build --locked
cargo clippy --locked -- -D warnings
cargo fmt --check
```

Needs: Rust stable (pinned by `../rust-toolchain.toml`), gcc,
pkg-config, libwayland-dev, libxkbcommon-dev, wayland-protocols. The
nested build does **not** need libudev-dev, libinput-dev, libgbm-dev,
or libseat-dev (those are Stage 3's standalone backend). The build
produces `target/debug/sevyn-compositor` (`target/release/` with
`--release`). The test client builds with gcc + wayland-scanner (see
the `compositor` CI job).
