# Genesis compositor (C, wlroots)

The SevynOS compositor. See `../COMPOSITOR_PLAN.md` for the full plan.

## Layout (ADR-0006 Phase 2)

```text
compositor/
├── include/sevyn/   # SevynOS-owned public headers (Stage 2; no wlr_* types)
├── src/             # server: main, server, backend, output, seat, input,
│                    #   xdg-shell, surface-registry, runtime-bridge, logging
├── tests/           # test clients (sevyn-test-client, the M1 client)
├── protocols/       # SevynOS-owned protocol XML (Stage 4 adoption)
└── meson.build
```

At seed time only `tests/test-client.c` exists; Stage 2 lands the server
in `src/` and the public headers in `include/sevyn/`. The server is C
(ADR-0006); the Shell stays TypeScript/React Native and talks to it over
the Runtime IPC socket, never over raw wlroots pointers.

## Build

```sh
meson setup build .
meson compile -C build
```

Needs: gcc, pkg-config, libwayland-dev, wayland-protocols, meson,
ninja. The build produces `build/sevyn-test-client`. `meson.build` is
the entry point the SevynOS image builder uses (pinned commit, see the
root README).
